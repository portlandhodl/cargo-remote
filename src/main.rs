mod agent;
mod cli;
mod config;
mod progress;
mod project;
mod rsync;
mod runner;
mod ssh;
mod sshcfg;
mod transfer;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, RemoteOpts, TransferMode};
use config::Config;
use project::{CargoSubcommand, Project};
use runner::{Context, Requirements, Runner};
use transfer::{Compressor, Transfer};

#[tokio::main]
async fn main() -> ExitCode {
    // When run as `cargo remote ...`, cargo executes us as `cargo-remote remote ...`,
    // injecting the subcommand name as argv[1]. Strip it so both invocation styles work:
    //   cargo remote build -r host      (via cargo, like `cargo nextest run`)
    //   cargo-remote build -r host      (direct)
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let argv: Vec<std::ffi::OsString> = match argv.get(1).and_then(|a| a.to_str()) {
        Some("remote") => argv
            .iter()
            .take(1)
            .chain(argv.iter().skip(2))
            .cloned()
            .collect(),
        _ => argv,
    };
    let cli = Cli::parse_from(argv);

    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("cargo-remote: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<u8> {
    match cli.command {
        Command::Agent { version } => {
            if version {
                println!("{}", agent::version_string());
                return Ok(0);
            }
            agent::serve().await?;
            Ok(0)
        }
        Command::Hosts => {
            for h in sshcfg::config_hosts()? {
                println!("{h}");
            }
            Ok(0)
        }
        Command::Sync { opts } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.sync().await
        }
        Command::Build { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Build, &args).await
        }
        Command::Check { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Check, &args).await
        }
        Command::Clippy { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Clippy, &args).await
        }
        Command::Test { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Test, &args).await
        }
        Command::Run { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Run, &args).await
        }
        Command::Bench { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Bench, &args).await
        }
        Command::Doc { opts, args } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.build(CargoSubcommand::Doc, &args).await
        }
        Command::Clean { opts } => {
            let ctx = build_context(&cli.opts, &opts, true).await?;
            ctx.clean().await
        }
        Command::Probe { opts } => {
            // Probe connects itself, step by step, so a connect failure is
            // reported as a check result instead of a hard error.
            let ctx = build_context(&cli.opts, &opts, false).await?;
            ctx.probe().await
        }
        Command::Ext(argv) => {
            // Cargo extensions (e.g. nextest): only global options apply,
            // everything after the subcommand goes to the remote cargo.
            let ctx = build_context(&cli.opts, &RemoteOpts::default(), true).await?;
            ctx.ext(&argv).await
        }
    }
}

/// Resolve configuration from all sources (lowest to highest precedence:
/// global config file, project config files, environment, CLI flags),
/// connect (unless `connect` is false, e.g. `probe` connects itself), and
/// assemble the run context.
async fn build_context(global: &RemoteOpts, local: &RemoteOpts, connect: bool) -> anyhow::Result<Context> {
    let opts = global.merge(local);

    let project = Project::discover(std::env::current_dir()?)?;

    let cfg = Config::load(&project.root)?;

    let host = opts
        .remote
        .clone()
        .or_else(|| std::env::var("CARGO_REMOTE_HOST").ok())
        .or_else(|| cfg.host.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no remote host given.\n\
                 Use -r <HOST>, set CARGO_REMOTE_HOST, or put `host = \"...\"` in \
                 .config/cargo-remote.toml\n\
                 (or ~/.config/cargo-remote/config.toml for all projects).\n\
                 Run `cargo remote hosts` to see host names from your ~/.ssh/config."
            )
        })?;

    let remote_dir = opts
        .remote_dir
        .clone()
        .or_else(|| cfg.remote_dir.clone())
        .unwrap_or_else(|| "~/remote-builds".to_string());

    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    // Live sync status only on an interactive stderr, unless disabled by
    // flag or config.
    let sync_status = !opts.no_sync_status
        && cfg.sync_status.unwrap_or(true)
        && std::io::stderr().is_terminal();

    let transfer = match opts.transfer.or(cfg.transfer).unwrap_or_default() {
        TransferMode::Auto => Transfer::Auto,
        TransferMode::Rsync | TransferMode::RsyncZ => Transfer::Delta,
        TransferMode::Tar => Transfer::Tar {
            compressor: Compressor::None,
        },
        TransferMode::TarGz => Transfer::Tar {
            compressor: Compressor::Gzip,
        },
        TransferMode::TarZstd => Transfer::Tar {
            compressor: Compressor::Zstd,
        },
        TransferMode::TarInc => Transfer::TarIncremental,
    };

    // Dry-run never touches the network; probe connects on its own.
    let ssh = if opts.dry_run || !connect {
        None
    } else {
        Some(ssh::Ssh::connect(&host, interactive).await?)
    };

    let toolchain = opts.toolchain.clone().or_else(|| cfg.toolchain.clone());
    let cargo_path = cfg.cargo_path.clone().unwrap_or_else(default_cargo_path);
    let env = [cfg.env.clone().unwrap_or_default(), opts.env.clone()].concat();

    // Announce where and how we're about to build before anything is sent.
    if let Some(ssh) = &ssh {
        let rustflags = std::env::var("RUSTFLAGS").ok().filter(|s| !s.is_empty());
        for line in host_status_lines(
            &host,
            ssh.info(),
            &remote_dir,
            &transfer,
            &cargo_path,
            toolchain.as_deref(),
            &env,
            &cfg,
            rustflags.as_deref(),
        ) {
            eprintln!("{line}");
        }
    }

    let mut ctx = Context::new(
        Runner {
            dry_run: opts.dry_run,
            verbose: opts.verbose || cfg.verbose.unwrap_or(false),
            sync_status,
        },
        project,
        host,
        remote_dir,
        transfer,
        opts.copy_back.or(cfg.copy_back).unwrap_or(true),
        !opts.no_sync && cfg.sync.unwrap_or(true),
        opts.exclude_git || cfg.exclude_git.unwrap_or(false),
        [cfg.exclude.clone().unwrap_or_default(), opts.exclude.clone()].concat(),
        env,
        toolchain,
        interactive,
        cargo_path,
        ssh,
    );
    ctx.requirements = Requirements {
        arch: cfg.arch.clone(),
        min_rust_version: cfg.min_rust_version.clone(),
    };
    Ok(ctx)
}

/// One-time status lines before any transfer: which host (alias + resolved
/// address), what effective config, and which env vars will be sent over.
#[allow(clippy::too_many_arguments)]
fn host_status_lines(
    host: &str,
    info: &ssh::ConnInfo,
    remote_dir: &str,
    transfer: &Transfer,
    cargo_path: &str,
    toolchain: Option<&str>,
    env: &[String],
    cfg: &Config,
    rustflags: Option<&str>,
) -> Vec<String> {
    let mut line = format!(
        "cargo-remote: host {host} ({}@{}:{}), dir {remote_dir}, transfer {}",
        info.user,
        info.host,
        info.port,
        transfer.name()
    );
    line.push_str(&format!(", cargo {cargo_path}"));
    if let Some(tc) = toolchain {
        line.push_str(&format!(" +{tc}"));
    }
    let mut reqs = Vec::new();
    if let Some(a) = &cfg.arch {
        reqs.push(format!("arch {a}"));
    }
    if let Some(v) = &cfg.min_rust_version {
        reqs.push(format!("cargo >= {v}"));
    }
    if !reqs.is_empty() {
        line.push_str(&format!(", requires {}", reqs.join(", ")));
    }

    let mut lines = vec![line];
    // The exact env that will prefix the remote cargo command: the
    // configured pairs plus RUSTFLAGS when forwarded from the local env.
    let mut sent: Vec<String> = Vec::new();
    if let Some(rf) = rustflags {
        sent.push(format!("RUSTFLAGS={rf}"));
    }
    sent.extend(env.iter().cloned());
    if !sent.is_empty() {
        lines.push(format!("cargo-remote: env sent: {}", sent.join(" ")));
    }
    lines
}

fn default_cargo_path() -> String {
    std::env::var("CARGO_REMOTE_CARGO").unwrap_or_else(|_| "$HOME/.cargo/bin/cargo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> ssh::ConnInfo {
        ssh::ConnInfo {
            host: "10.0.0.2".to_string(),
            port: 22,
            user: "me".to_string(),
        }
    }

    #[test]
    fn status_lines_show_host_config_constraints_and_env() {
        let cfg: Config = toml::from_str("arch = \"host\"\nmin_rust_version = \"1.85\"").unwrap();
        let lines = host_status_lines(
            "alias",
            &conn(),
            "~/remote-builds/x",
            &Transfer::Auto,
            "$HOME/.cargo/bin/cargo",
            Some("nightly"),
            &["FOO=bar".to_string()],
            &cfg,
            Some("-Ctarget-cpu=native"),
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("host alias (me@10.0.0.2:22)"), "{}", lines[0]);
        assert!(lines[0].contains("transfer auto"), "{}", lines[0]);
        assert!(
            lines[0].contains("cargo $HOME/.cargo/bin/cargo +nightly"),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("requires arch host, cargo >= 1.85"),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "cargo-remote: env sent: RUSTFLAGS=-Ctarget-cpu=native FOO=bar"
        );
    }

    #[test]
    fn status_lines_omit_empty_sections() {
        let cfg = Config::default();
        let lines = host_status_lines(
            "alias",
            &conn(),
            "~/remote-builds/x",
            &Transfer::Delta,
            "cargo",
            None,
            &[],
            &cfg,
            None,
        );
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("requires"), "{}", lines[0]);
        assert!(lines[0].contains("transfer delta"), "{}", lines[0]);
    }
}
