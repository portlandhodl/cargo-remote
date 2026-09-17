//! Configuration files: persistent defaults so `cargo remote` needs no flags.
//!
//! Files, from lowest to highest precedence:
//!   1. `$XDG_CONFIG_HOME/cargo-remote/config.toml` (or `~/.config/...`) —
//!      global defaults for all projects (e.g. your default build host).
//!   2. `.config/cargo-remote.toml` — project config, nextest-style.
//!   3. `.cargo-remote.toml` — project config at the workspace root.
//!
//! `$CARGO_REMOTE_HOST` and CLI flags sit above all files. Scalar fields from
//! higher-precedence files override lower ones; list fields (`exclude`,
//! `env`) accumulate. Unknown keys are rejected so typos surface.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::cli::TransferMode;

/// All fields optional; an unset field means "fall through to the next
/// source, then to the built-in default".
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Remote host: an alias from ~/.ssh/config or a plain host name.
    pub host: Option<String>,
    /// Base directory on the remote holding per-project build dirs.
    pub remote_dir: Option<String>,
    /// Path to the cargo binary on the remote.
    pub cargo_path: Option<String>,
    /// Source transfer mode (auto, rsync, tar, tar-gz, tar-zstd, tar-inc).
    pub transfer: Option<TransferMode>,
    /// Copy built binaries back into local target/ after `build`.
    pub copy_back: Option<bool>,
    /// Sync sources before running the remote command.
    pub sync: Option<bool>,
    /// Also exclude .git from the sync.
    pub exclude_git: Option<bool>,
    /// Extra root-anchored paths to exclude from the sync.
    pub exclude: Option<Vec<String>>,
    /// Environment variables for the remote build, "KEY=VALUE".
    pub env: Option<Vec<String>>,
    /// Rust toolchain on the remote (via rustup shim), e.g. "nightly".
    pub toolchain: Option<String>,
    /// Verbose logging: echo each step/command (logs on or off).
    pub verbose: Option<bool>,
    /// Live sync status line on stderr.
    pub sync_status: Option<bool>,
    /// Required remote machine architecture (`uname -m`), e.g. "x86_64".
    /// The special value "host" means the local machine's arch, so copied-
    /// back binaries are guaranteed to match this machine.
    pub arch: Option<String>,
    /// Minimum acceptable remote toolchain version, e.g. "1.85".
    pub min_rust_version: Option<String>,
}

impl Config {
    /// Load and merge all config sources for `project_root`.
    pub fn load(project_root: &Path) -> Result<Config> {
        Self::load_from(project_root, global_path().as_deref())
    }

    /// Merge global < project .config < project root, for tests and [`load`].
    pub fn load_from(project_root: &Path, global: Option<&Path>) -> Result<Config> {
        let mut cfg = Config::default();
        if let Some(p) = global {
            cfg = cfg.overlay(read(p)?);
        }
        cfg = cfg.overlay(read(&project_root.join(".config/cargo-remote.toml"))?);
        cfg = cfg.overlay(read(&project_root.join(".cargo-remote.toml"))?);
        Ok(cfg)
    }

    /// Overlay `other` (higher precedence) onto `self`: set scalar fields
    /// win, list fields accumulate (self first so later sources can
    /// override earlier assignments on the remote shell line).
    fn overlay(self, o: Option<Config>) -> Config {
        let Some(o) = o else { return self };
        Config {
            host: o.host.or(self.host),
            remote_dir: o.remote_dir.or(self.remote_dir),
            cargo_path: o.cargo_path.or(self.cargo_path),
            transfer: o.transfer.or(self.transfer),
            copy_back: o.copy_back.or(self.copy_back),
            sync: o.sync.or(self.sync),
            exclude_git: o.exclude_git.or(self.exclude_git),
            exclude: concat(self.exclude, o.exclude),
            env: concat(self.env, o.env),
            toolchain: o.toolchain.or(self.toolchain),
            verbose: o.verbose.or(self.verbose),
            sync_status: o.sync_status.or(self.sync_status),
            arch: o.arch.or(self.arch),
            min_rust_version: o.min_rust_version.or(self.min_rust_version),
        }
    }
}

fn concat(low: Option<Vec<String>>, high: Option<Vec<String>>) -> Option<Vec<String>> {
    match (low, high) {
        (None, None) => None,
        (a, b) => Some([a.unwrap_or_default(), b.unwrap_or_default()].concat()),
    }
}

/// Global config path: `$XDG_CONFIG_HOME/cargo-remote/config.toml`, falling
/// back to `~/.config/cargo-remote/config.toml`.
pub fn global_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("cargo-remote/config.toml"));
    }
    std::env::home_dir().map(|h| h.join(".config/cargo-remote/config.toml"))
}

fn read(path: &Path) -> Result<Option<Config>> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let cfg: Config = toml::from_str(
            r#"
host = "builder"
remote_dir = "~/builds"
cargo_path = "/usr/local/bin/cargo"
transfer = "tar-gz"
copy_back = false
sync = false
exclude_git = true
exclude = ["data", "fixtures"]
env = ["FOO=bar", "BAZ=qux"]
toolchain = "nightly"
verbose = true
sync_status = false
arch = "host"
min_rust_version = "1.85"
"#,
        )
        .unwrap();
        assert_eq!(cfg.host.as_deref(), Some("builder"));
        assert_eq!(cfg.remote_dir.as_deref(), Some("~/builds"));
        assert_eq!(cfg.cargo_path.as_deref(), Some("/usr/local/bin/cargo"));
        assert_eq!(cfg.transfer, Some(TransferMode::TarGz));
        assert_eq!(cfg.copy_back, Some(false));
        assert_eq!(cfg.sync, Some(false));
        assert_eq!(cfg.exclude_git, Some(true));
        assert_eq!(cfg.exclude.as_ref().unwrap().len(), 2);
        assert_eq!(cfg.env.as_ref().unwrap()[1], "BAZ=qux");
        assert_eq!(cfg.toolchain.as_deref(), Some("nightly"));
        assert_eq!(cfg.verbose, Some(true));
        assert_eq!(cfg.sync_status, Some(false));
        assert_eq!(cfg.arch.as_deref(), Some("host"));
        assert_eq!(cfg.min_rust_version.as_deref(), Some("1.85"));
    }

    #[test]
    fn transfer_mode_names_match_cli() {
        // The TOML spelling is the same kebab-case name the CLI accepts.
        for (text, mode) in [
            ("auto", TransferMode::Auto),
            ("rsync", TransferMode::Rsync),
            ("rsync-z", TransferMode::RsyncZ),
            ("tar", TransferMode::Tar),
            ("tar-gz", TransferMode::TarGz),
            ("tar-zstd", TransferMode::TarZstd),
            ("tar-inc", TransferMode::TarInc),
        ] {
            let cfg: Config = toml::from_str(&format!("transfer = \"{text}\"")).unwrap();
            assert_eq!(cfg.transfer, Some(mode), "{text}");
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("hostname = \"oops\"").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn overlay_scalars_high_wins_lists_accumulate() {
        let low: Config = toml::from_str(
            r#"host = "global-host"
verbose = true
exclude = ["a"]
env = ["A=1"]
"#,
        )
        .unwrap();
        let high: Config = toml::from_str(
            r#"host = "project-host"
exclude = ["b"]
env = ["A=2"]
"#,
        )
        .unwrap();
        let merged = low.overlay(Some(high));
        assert_eq!(merged.host.as_deref(), Some("project-host"));
        assert_eq!(merged.verbose, Some(true)); // kept from lower file
        assert_eq!(
            merged.exclude.unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(merged.env.unwrap(), vec!["A=1".to_string(), "A=2".to_string()]);
    }

    #[test]
    fn load_from_orders_precedence() {
        let dir = std::env::temp_dir().join(format!("cargo-remote-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        let global = dir.join("global.toml");
        std::fs::write(&global, "host = \"global\"\nverbose = true\n").unwrap();
        std::fs::write(dir.join(".config/cargo-remote.toml"), "host = \"dot-config\"\n").unwrap();
        std::fs::write(dir.join(".cargo-remote.toml"), "remote_dir = \"~/x\"\n").unwrap();

        let cfg = Config::load_from(&dir, Some(&global)).unwrap();
        // root file has no host -> .config wins over global
        assert_eq!(cfg.host.as_deref(), Some("dot-config"));
        assert_eq!(cfg.remote_dir.as_deref(), Some("~/x"));
        assert_eq!(cfg.verbose, Some(true));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
