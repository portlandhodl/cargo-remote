use clap::{Args, Parser, Subcommand, ValueEnum};

/// Compile Rust projects on a remote server over SSH.
///
/// Sources are synced to the server (incrementally, so repeat runs are fast)
/// and the cargo command runs there. Use the same host names as in your
/// ~/.ssh/config — run `cargo remote hosts` to list them.
///
/// Examples:
///   cargo remote -r my-server build --release
///   cargo remote build -r my-server -- --features foo
///   cargo remote -r my-server test
///   cargo remote hosts
#[derive(Parser, Debug)]
#[command(name = "cargo-remote", bin_name = "cargo remote", version, about, long_about)]
pub struct Cli {
    #[command(flatten)]
    pub opts: RemoteOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sync sources and run `cargo build` on the remote; copy resulting binaries back.
    Build {
        #[command(flatten)]
        opts: RemoteOpts,
        /// Extra arguments passed through to cargo.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync sources and run `cargo check` on the remote.
    Check {
        #[command(flatten)]
        opts: RemoteOpts,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync sources and run `cargo clippy` on the remote.
    Clippy {
        #[command(flatten)]
        opts: RemoteOpts,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync sources and run `cargo test` on the remote (tests run on the server).
    Test {
        #[command(flatten)]
        opts: RemoteOpts,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync sources and run `cargo run` on the remote (binary runs on the server).
    Run {
        #[command(flatten)]
        opts: RemoteOpts,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync sources and run `cargo bench` on the remote.
    Bench {
        #[command(flatten)]
        opts: RemoteOpts,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync sources and run `cargo doc` on the remote.
    Doc {
        #[command(flatten)]
        opts: RemoteOpts,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run `cargo clean` on the remote (clears the remote build cache).
    Clean {
        #[command(flatten)]
        opts: RemoteOpts,
    },
    /// Only sync sources to the remote (pre-warm the cache, no build).
    Sync {
        #[command(flatten)]
        opts: RemoteOpts,
    },
    /// List host names found in ~/.ssh/config.
    Hosts,
    /// (internal) Run as the remote sync agent. Not for direct use.
    #[command(hide = true, name = "__agent")]
    Agent {
        /// Print the agent protocol version and exit.
        #[arg(long)]
        version: bool,
    },
}

/// Options that can appear either globally (before the subcommand) or on the
/// subcommand itself. Subcommand values win.
#[derive(Args, Debug, Default, Clone)]
pub struct RemoteOpts {
    /// SSH host, as named in ~/.ssh/config.
    ///
    /// Falls back to $CARGO_REMOTE_HOST, then `host` in .cargo-remote.toml.
    #[arg(short, long, value_name = "HOST", global = false)]
    pub remote: Option<String>,

    /// Base directory on the remote that holds per-project build dirs.
    #[arg(long, value_name = "DIR", default_value = None)]
    pub remote_dir: Option<String>,

    /// How to ship sources to the remote.
    ///
    /// auto: tar stream when the remote has no copy yet, delta sync afterwards.
    #[arg(long, value_enum, value_name = "MODE", default_value = None)]
    pub transfer: Option<TransferMode>,

    /// Copy built binaries back into the local target/ dir after `build`.
    #[arg(long, overrides_with = "no_copy_back", default_value = None)]
    pub copy_back: Option<bool>,

    /// Don't copy anything back after a build.
    #[arg(long, overrides_with = "copy_back")]
    pub no_copy_back: bool,

    /// Skip the source sync (use whatever is already on the server).
    #[arg(long)]
    pub no_sync: bool,

    /// Also exclude the .git directory from the sync.
    #[arg(long)]
    pub exclude_git: bool,

    /// Extra path (relative to project root) to exclude from the sync.
    /// May be repeated.
    #[arg(long, value_name = "PATH")]
    pub exclude: Vec<String>,

    /// Environment variable to set on the remote build, KEY=VALUE.
    /// May be repeated. (RUSTFLAGS is forwarded automatically if set locally.)
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Rust toolchain to use on the remote (e.g. "nightly"), via rustup shim.
    #[arg(long, value_name = "NAME")]
    pub toolchain: Option<String>,

    /// Print the commands that would run without executing them.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Echo each command as it runs.
    #[arg(short, long)]
    pub verbose: bool,
}

impl RemoteOpts {
    /// Subcommand options take precedence over global ones.
    pub fn merge(&self, other: &RemoteOpts) -> RemoteOpts {
        RemoteOpts {
            remote: other.remote.clone().or_else(|| self.remote.clone()),
            remote_dir: other.remote_dir.clone().or_else(|| self.remote_dir.clone()),
            transfer: other.transfer.or(self.transfer),
            copy_back: other.copy_back.or(self.copy_back),
            no_sync: other.no_sync || self.no_sync,
            exclude_git: other.exclude_git || self.exclude_git,
            exclude: [self.exclude.clone(), other.exclude.clone()].concat(),
            env: [self.env.clone(), other.env.clone()].concat(),
            toolchain: other.toolchain.clone().or_else(|| self.toolchain.clone()),
            dry_run: other.dry_run || self.dry_run,
            verbose: other.verbose || self.verbose,
            no_copy_back: other.no_copy_back || self.no_copy_back,
        }
        .apply_no_copy_back()
    }

    fn apply_no_copy_back(mut self) -> Self {
        if self.no_copy_back {
            self.copy_back = Some(false);
        }
        self
    }
}

#[derive(ValueEnum, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    /// Pick automatically: tar-gz stream when the remote dir is empty, delta sync after.
    #[default]
    Auto,
    /// Built-in delta sync via the remote agent (rsync algorithm, no rsync binary).
    Rsync,
    /// Same as rsync (accepted for compatibility).
    RsyncZ,
    /// Stream an uncompressed tar archive over ssh (always full).
    Tar,
    /// Stream a gzip-compressed tar archive over ssh (always full).
    TarGz,
    /// Stream a zstd-compressed tar archive over ssh (always full).
    TarZstd,
    /// GNU tar incremental archives (snapshot file kept locally).
    TarInc,
}
