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

    let outcome = Config::load(&project.root)?;
    for w in &outcome.warnings {
        eprintln!("cargo-remote: warning: {w}");
    }
    let cfg = outcome.config;

    // CLI beats env beats config. `host` in a project-level config file is
    // honored only with trust_project_config = true in the global config,
    // and marks the host as less trustworthy (copy-back defaults off).
    let cli_or_env = opts
        .remote
        .clone()
        .or_else(|| std::env::var("CARGO_REMOTE_HOST").ok());
    let host_from_project = cli_or_env.is_none() && outcome.host_from_project;
    let host = cli_or_env
        .or_else(|| cfg.host.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no remote host given.\n\
                 Use -r <HOST>, set CARGO_REMOTE_HOST, or put `host = \"...\"` in \
                 ~/.config/cargo-remote/config.toml.\n\
                 Run `cargo remote hosts` to see host names from your ~/.ssh/config."
            )
        })?;

    let remote_dir = opts
        .remote_dir
        .clone()
        .or_else(|| cfg.remote_dir.clone())
        .unwrap_or_else(|| "~/remote-builds".to_string());

    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let verbose = opts.verbose || cfg.verbose.unwrap_or(false);

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

    // These values end up inside remote shell command lines, so validate
    // them here (fail fast, before any network activity) regardless of the
    // quoting applied at the sinks.
    let toolchain = opts.toolchain.clone().or_else(|| cfg.toolchain.clone());
    if let Some(tc) = &toolchain {
        anyhow::ensure!(
            runner::is_valid_toolchain(tc),
            "invalid toolchain name '{tc}': expected [A-Za-z0-9._+-]+"
        );
    }
    let cargo_path = cfg.cargo_path.clone().unwrap_or_else(default_cargo_path);
    anyhow::ensure!(
        runner::is_valid_cargo_path(&cargo_path),
        "invalid cargo_path '{cargo_path}': expected a plain path over \
         [A-Za-z0-9._+/-] (an optional ~/ or $HOME/ prefix is allowed)"
    );
    let env = [cfg.env.clone().unwrap_or_default(), opts.env.clone()].concat();
    runner::validate_env_entries(&env)?;
    let rustflags = std::env::var("RUSTFLAGS").ok().filter(|s| !s.is_empty());

    // Dry-run never touches the network; probe connects on its own.
    let ssh = if opts.dry_run || !connect {
        None
    } else {
        Some(ssh::Ssh::connect(&host, interactive).await?)
    };

    // Announce where and how we're about to build before anything is sent.
    if let Some(ssh) = &ssh {
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
            verbose,
        ) {
            eprintln!("{line}");
        }
    }

    // Copy-back defaults to off when the host came from a (trusted) project
    // config file; an explicit --copy-back or copy_back in the global
    // config forces it.
    let copy_back = match opts.copy_back.or(cfg.copy_back) {
        Some(v) => v,
        None if host_from_project => {
            eprintln!(
                "cargo-remote: warning: copy-back disabled — the host came from a \
                 project config file; pass --copy-back (or set copy_back in your \
                 global config) to force it on"
            );
            false
        }
        None => true,
    };

    let mut ctx = Context::new(
        Runner {
            dry_run: opts.dry_run,
            verbose,
            sync_status,
        },
        project,
        host,
        remote_dir,
        transfer,
        copy_back,
        opts.update_lockfile,
        !opts.no_sync && cfg.sync.unwrap_or(true),
        opts.exclude_git || cfg.exclude_git.unwrap_or(false),
        [cfg.exclude.clone().unwrap_or_default(), opts.exclude.clone()].concat(),
        env,
        rustflags,
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
    verbose: bool,
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
    // The env that will prefix the remote cargo command: the configured
    // pairs (values masked — they are often secrets; -v shows them) plus
    // RUSTFLAGS when forwarded from the local env.
    let mut sent: Vec<String> = Vec::new();
    if let Some(rf) = rustflags {
        sent.push(format!("RUSTFLAGS={rf}"));
    }
    sent.extend(env.iter().map(|kv| {
        if verbose {
            kv.clone()
        } else {
            runner::mask_env(kv)
        }
    }));
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
            false,
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
        // Env values are masked by default; RUSTFLAGS (flags, not secrets)
        // stays visible.
        assert_eq!(
            lines[1],
            "cargo-remote: env sent: RUSTFLAGS=-Ctarget-cpu=native FOO=***"
        );
    }

    #[test]
    fn status_lines_reveal_env_values_in_verbose() {
        let cfg = Config::default();
        let lines = host_status_lines(
            "alias",
            &conn(),
            "~/b",
            &Transfer::Auto,
            "cargo",
            None,
            &["FOO=bar".to_string(), "TOKEN=s3cret".to_string()],
            &cfg,
            None,
            true,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "cargo-remote: env sent: FOO=bar TOKEN=s3cret");
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
            false,
        );
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("requires"), "{}", lines[0]);
        assert!(lines[0].contains("transfer delta"), "{}", lines[0]);
    }
}
