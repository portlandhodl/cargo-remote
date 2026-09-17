//! Source-tree transfer strategies — all in-process over the built-in SSH
//! client, no external `ssh` or `rsync` binaries.
//!
//! Two families:
//!  * delta sync via the remote agent ([`crate::agent`]): rsync's rolling-
//!    checksum algorithm in pure Rust; only changed blocks cross the wire.
//!  * streamed archive (tar over the channel): one stream, no per-file
//!    overhead; the universal fallback when the agent can't run (e.g. the
//!    remote has a different CPU/OS than the local machine).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::progress::Progress;
use crate::runner::{Context, shell_join};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compressor {
    None,
    Gzip,
    Zstd,
}

impl Compressor {
    /// Local pipeline stage compressing stdin -> stdout ("" for none).
    fn compress_prog(&self) -> &'static str {
        match self {
            Compressor::None => "",
            Compressor::Gzip => "gzip",
            Compressor::Zstd => "zstd",
        }
    }
    /// Remote pipeline stage decompressing stdin -> stdout ("" for none).
    fn decompress_stage(&self) -> &'static str {
        match self {
            Compressor::None => "",
            Compressor::Gzip => "gzip -dc",
            Compressor::Zstd => "zstd -dc",
        }
    }
}

/// How to ship sources to the remote.
pub enum Transfer {
    /// tar stream when the remote has no copy yet, agent delta sync after.
    Auto,
    /// Agent delta sync (the rsync algorithm, built in).
    Delta,
    /// Stream a tar archive (optionally compressed), always full.
    Tar { compressor: Compressor },
    /// GNU tar incremental archives (local snapshot file).
    TarIncremental,
}

impl Transfer {
    /// Short name for status display ("tar-gz", "delta", ...).
    pub fn name(&self) -> &'static str {
        match self {
            Transfer::Auto => "auto",
            Transfer::Delta => "delta",
            Transfer::Tar {
                compressor: Compressor::None,
            } => "tar",
            Transfer::Tar {
                compressor: Compressor::Gzip,
            } => "tar-gz",
            Transfer::Tar {
                compressor: Compressor::Zstd,
            } => "tar-zstd",
            Transfer::TarIncremental => "tar-inc",
        }
    }

    pub async fn sync(&self, ctx: &Context) -> Result<()> {
        match self {
            Transfer::Auto => auto_sync(ctx).await,
            Transfer::Delta => delta_sync(ctx).await,
            Transfer::Tar { compressor } => tar_sync(ctx, *compressor, None).await,
            Transfer::TarIncremental => {
                let snar = snapshot_path(&ctx.host, &ctx.project.name)?;
                if !ctx.runner.dry_run
                    && let Some(dir) = snar.parent()
                {
                    std::fs::create_dir_all(dir)?;
                }
                tar_sync(ctx, Compressor::None, Some(&snar)).await
            }
        }
    }
}

fn snapshot_path(host: &str, project: &str) -> Result<PathBuf> {
    let sane: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let home =
        std::env::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home
        .join(".cache/cargo-remote/inc")
        .join(format!("{sane}-{project}.snar")))
}

/// Delta sync through the remote agent; falls back to incremental tar when
/// the agent can't run on the remote (different OS/arch).
async fn delta_sync(ctx: &Context) -> Result<()> {
    if ctx.runner.dry_run {
        ctx.runner.show_line(&format!(
            "sync {} -> {}:{} (delta agent, tar fallback)",
            ctx.project.root.display(),
            ctx.host,
            ctx.remote_dir()
        ));
        return Ok(());
    }
    match ctx.ensure_agent().await? {
        Some(agent_q) => {
            let mut agent = Agent::start(ctx.require_ssh()?, &agent_q).await?;
            let mut progress = Progress::new("sync", &ctx.runner);
            let stats = agent
                .sync(
                    &ctx.project.root,
                    &ctx.remote_dir(),
                    &ctx.excludes(),
                    true,
                    &mut progress,
                )
                .await?;
            agent.quit().await?;
            progress.finish(&format!(
                "{} files changed, {} sent ({} full size)",
                stats.files,
                human(stats.sent_bytes),
                human(stats.full_bytes)
            ));
            Ok(())
        }
        None => {
            if ctx.runner.verbose {
                eprintln!("cargo-remote: agent unavailable on remote; using incremental tar");
            }
            let snar = snapshot_path(&ctx.host, &ctx.project.name)?;
            if let Some(dir) = snar.parent() {
                std::fs::create_dir_all(dir)?;
            }
            tar_sync(ctx, Compressor::None, Some(&snar)).await
        }
    }
}

/// Cold remote -> single tar-gz stream; warm remote -> delta sync.
async fn auto_sync(ctx: &Context) -> Result<()> {
    if ctx.runner.dry_run {
        ctx.runner.show_line(&format!(
            "auto: probe {}:{}, then delta-sync (warm) or tar-gz stream (cold)",
            ctx.host,
            ctx.remote_dir()
        ));
        return Ok(());
    }
    let probe = format!("test -d {}", ctx.remote_dir_q());
    let (code, _) = ctx.require_ssh()?.exec_capture(&probe).await?;
    if code == 0 {
        if ctx.runner.verbose {
            eprintln!("auto: remote dir exists -> delta sync");
        }
        delta_sync(ctx).await
    } else {
        if ctx.runner.verbose {
            eprintln!("auto: cold remote -> tar-gz stream");
        }
        tar_sync(ctx, Compressor::Gzip, None).await
    }
}

/// Stream a tar archive over an exec channel:
/// `tar [excludes] -C ROOT -cf - . | [compress]`  ==>  remote `mkdir -p D && [decompress |] tar -x`
async fn tar_sync(
    ctx: &Context,
    compressor: Compressor,
    incremental_snapshot: Option<&Path>,
) -> Result<()> {
    let mut tar_args: Vec<String> = Vec::new();
    if let Some(snar) = incremental_snapshot {
        tar_args.push(format!("--listed-incremental={}", snar.display()));
    }
    for e in ctx.excludes() {
        tar_args.push(format!("--exclude=./{e}"));
    }
    tar_args.push("-C".into());
    tar_args.push(ctx.project.root.display().to_string());
    tar_args.push("-cf".into());
    tar_args.push("-".into());
    tar_args.push(".".into());

    let mut remote = format!("mkdir -p {}", ctx.remote_dir_q());
    remote.push_str(" && ");
    if incremental_snapshot.is_some() {
        // --listed-incremental=/dev/null processes deletions from the archive.
        remote.push_str("tar -x --listed-incremental=/dev/null -f -");
    } else {
        let de = compressor.decompress_stage();
        if de.is_empty() {
            remote.push_str("tar -xf -");
        } else {
            remote.push_str(&format!("{de} | tar -xf -"));
        }
    }
    remote.push_str(&format!(" -C {}", ctx.remote_dir_q()));

    let mut local_line = format!("tar {}", shell_join(tar_args.iter().map(|s| s.as_str())));
    let comp = compressor.compress_prog();
    if incremental_snapshot.is_none() && !comp.is_empty() {
        local_line.push_str(&format!(" | {comp} -c"));
    }

    if ctx.runner.dry_run {
        ctx.runner
            .show_line(&format!("{local_line}  ==>  ssh-exec {}: {remote}", ctx.host));
        return Ok(());
    }

    let ssh = ctx.require_ssh()?;
    let (tx, rx) = mpsc::channel::<bytes::Bytes>(8);
    let incremental = incremental_snapshot.is_some();
    let label = if incremental {
        "sync (tar-inc)".to_string()
    } else {
        match compressor {
            Compressor::None => "sync (tar)".to_string(),
            Compressor::Gzip => "sync (tar-gz)".to_string(),
            Compressor::Zstd => "sync (tar-zstd)".to_string(),
        }
    };
    let progress = Progress::new(&label, &ctx.runner);
    let producer =
        std::thread::spawn(move || run_tar_pipeline(tar_args, compressor, incremental, tx, progress));
    let code = ssh.exec_send(&remote, rx).await?;
    producer
        .join()
        .map_err(|_| anyhow::anyhow!("tar pipeline thread panicked"))??;
    if code != 0 {
        bail!("tar transfer failed with exit code {code}");
    }
    Ok(())
}

/// Run the local `tar [| compress]` pipeline, streaming bytes into `tx`.
fn run_tar_pipeline(
    tar_args: Vec<String>,
    compressor: Compressor,
    incremental: bool,
    tx: mpsc::Sender<bytes::Bytes>,
    mut progress: Progress,
) -> Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut tar = Command::new("tar")
        .args(&tar_args)
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to run tar")?;

    let comp_prog = compressor.compress_prog();
    let compress = !incremental && !comp_prog.is_empty();
    let (mut reader, child): (Box<dyn Read + Send>, Option<std::process::Child>) =
        if compress {
            let mut c = Command::new(comp_prog)
                .arg("-c")
                .stdin(Stdio::from(tar.stdout.take().expect("piped")))
                .stdout(Stdio::piped())
                .spawn()
                .with_context(|| format!("failed to run {comp_prog}"))?;
            (
                Box::new(c.stdout.take().expect("piped")) as Box<dyn Read + Send>,
                Some(c),
            )
        } else {
            (
                Box::new(tar.stdout.take().expect("piped")) as Box<dyn Read + Send>,
                None,
            )
        };

    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        progress.add_wire(n as u64);
        if tx.blocking_send(bytes::Bytes::copy_from_slice(&buf[..n])).is_err() {
            break; // receiver gone (remote failed)
        }
    }
    drop(tx);
    let tar_status = tar.wait()?;
    if let Some(mut c) = child {
        let st = c.wait()?;
        anyhow::ensure!(st.success(), "{comp_prog} failed: {st}");
    }
    anyhow::ensure!(tar_status.success(), "tar failed: {tar_status}");
    progress.finish(&format!("{} streamed", human(total)));
    Ok(())
}

pub fn human(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / (1u64 << 10) as f64)
    } else {
        format!("{n} B")
    }
}
