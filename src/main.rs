mod agent;
mod cli;
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
use project::{CargoSubcommand, Project};
use runner::{Context, Runner};
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
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.sync().await
        }
        Command::Build { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Build, &args).await
        }
        Command::Check { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Check, &args).await
        }
        Command::Clippy { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Clippy, &args).await
        }
        Command::Test { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Test, &args).await
        }
        Command::Run { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Run, &args).await
        }
        Command::Bench { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Bench, &args).await
        }
        Command::Doc { opts, args } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.build(CargoSubcommand::Doc, &args).await
        }
        Command::Clean { opts } => {
            let ctx = build_context(&cli.opts, &opts).await?;
            ctx.clean().await
        }
    }
}

/// Merge global options (before the subcommand) with per-subcommand options,
/// resolve the host/config/project, connect, and assemble the run context.
async fn build_context(global: &RemoteOpts, local: &RemoteOpts) -> anyhow::Result<Context> {
    let opts = global.merge(local);

    let project = Project::discover(std::env::current_dir()?)?;

    let project_cfg = project.load_config()?;

    let host = opts
        .remote
        .clone()
        .or_else(|| std::env::var("CARGO_REMOTE_HOST").ok())
        .or(project_cfg.host)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no remote host given.\n\
                 Use -r <HOST>, set CARGO_REMOTE_HOST, or put `host = \"...\"` in .cargo-remote.toml.\n\
                 Run `cargo remote hosts` to see host names from your ~/.ssh/config."
            )
        })?;

    let remote_dir = opts
        .remote_dir
        .clone()
        .unwrap_or_else(|| "~/remote-builds".to_string());

    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    let transfer = match opts.transfer.unwrap_or_default() {
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

    // Dry-run never touches the network.
    let ssh = if opts.dry_run {
        None
    } else {
        Some(ssh::Ssh::connect(&host, interactive).await?)
    };

    Ok(Context::new(
        Runner {
            dry_run: opts.dry_run,
            verbose: opts.verbose,
        },
        project,
        host,
        remote_dir,
        transfer,
        opts.copy_back.unwrap_or(true),
        !opts.no_sync,
        opts.exclude_git,
        opts.exclude.clone(),
        opts.env.clone(),
        opts.toolchain.clone(),
        interactive,
        project_cfg.cargo_path.unwrap_or_else(default_cargo_path),
        ssh,
    ))
}

fn default_cargo_path() -> String {
    std::env::var("CARGO_REMOTE_CARGO").unwrap_or_else(|_| "$HOME/.cargo/bin/cargo".to_string())
}
