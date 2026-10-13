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
    /// Opt-in (global config only): also honor the remote-control keys
    /// (`host`, `remote_dir`, `cargo_path`, `copy_back`) from project-level
    /// config files. A project file can never grant this to itself.
    pub trust_project_config: Option<bool>,
}

/// Result of merging the configuration layers.
pub struct LoadOutcome {
    pub config: Config,
    /// Warnings about project-file keys that were ignored (one per file).
    pub warnings: Vec<String>,
    /// The effective `host` value originated from a project-level file.
    /// (Only possible when the global config sets trust_project_config.)
    pub host_from_project: bool,
}

/// Strip the remote-control keys from a project-level config file.
fn strip_project_keys(mut c: Config) -> (Config, Vec<&'static str>) {
    let mut stripped = Vec::new();
    if c.host.take().is_some() {
        stripped.push("host");
    }
    if c.remote_dir.take().is_some() {
        stripped.push("remote_dir");
    }
    if c.cargo_path.take().is_some() {
        stripped.push("cargo_path");
    }
    if c.copy_back.take().is_some() {
        stripped.push("copy_back");
    }
    // Never honored from a project file: a repo cannot grant itself trust.
    c.trust_project_config = None;
    (c, stripped)
}

impl Config {
    /// Load and merge all config sources for `project_root`.
    ///
    /// Trust model: the global config (~/.config/cargo-remote/config.toml)
    /// is fully trusted. Project-level files (`.config/cargo-remote.toml`,
    /// `.cargo-remote.toml` in the workspace root — typically committed to
    /// the repo) may set build-shaping keys (`toolchain`, `env`, `transfer`,
    /// `sync`, `exclude*`, `arch`, `min_rust_version`, display), but the
    /// remote-control keys — `host`, `remote_dir`, `cargo_path`, `copy_back`
    /// — are stripped unless the global config sets
    /// `trust_project_config = true`.
    pub fn load(project_root: &Path) -> Result<LoadOutcome> {
        Self::load_from(project_root, global_path().as_deref())
    }

    /// Merge global < project .config < project root, for tests and [`load`].
    pub fn load_from(project_root: &Path, global: Option<&Path>) -> Result<LoadOutcome> {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();
        let mut host_from_project = false;

        if let Some(p) = global {
            cfg = cfg.overlay(read(p)?);
        }
        let trusted = cfg.trust_project_config == Some(true);

        for rel in [".config/cargo-remote.toml", ".cargo-remote.toml"] {
            let path = project_root.join(rel);
            let Some(c) = read(&path)? else { continue };
            let c = if trusted {
                if c.host.is_some() {
                    host_from_project = true;
                }
                c
            } else {
                let (clean, stripped) = strip_project_keys(c);
                if !stripped.is_empty() {
                    warnings.push(format!(
                        "ignoring {} from {rel}: project config is untrusted \
                         (set trust_project_config = true in your global \
                         cargo-remote config to allow these keys)",
                        stripped.join(", ")
                    ));
                }
                clean
            };
            cfg = cfg.overlay(Some(c));
        }

        Ok(LoadOutcome {
            config: cfg,
            warnings,
            host_from_project,
        })
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
            trust_project_config: o.trust_project_config.or(self.trust_project_config),
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
    fn untrusted_project_config_loses_remote_control_keys() {
        let dir = std::env::temp_dir().join(format!("cargo-remote-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        let global = dir.join("global.toml");
        std::fs::write(&global, "host = \"global\"\nverbose = true\n").unwrap();
        std::fs::write(dir.join(".config/cargo-remote.toml"), "host = \"dot-config\"\n").unwrap();
        std::fs::write(dir.join(".cargo-remote.toml"), "remote_dir = \"~/x\"\n").unwrap();

        let out = Config::load_from(&dir, Some(&global)).unwrap();
        // Project-level `host`/`remote_dir` are stripped; global `host` wins.
        assert_eq!(out.config.host.as_deref(), Some("global"));
        assert_eq!(out.config.remote_dir, None);
        assert_eq!(out.config.verbose, Some(true));
        assert!(!out.host_from_project);
        assert_eq!(out.warnings.len(), 2, "{:?}", out.warnings);
        assert!(out.warnings[0].contains("host"), "{:?}", out.warnings);
        assert!(out.warnings[1].contains("remote_dir"), "{:?}", out.warnings);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trusted_project_config_keeps_remote_control_keys() {
        let dir = std::env::temp_dir().join(format!("cargo-remote-cfg2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        let global = dir.join("global.toml");
        std::fs::write(&global, "trust_project_config = true\n").unwrap();
        std::fs::write(dir.join(".config/cargo-remote.toml"), "host = \"dot-config\"\n").unwrap();
        std::fs::write(dir.join(".cargo-remote.toml"), "remote_dir = \"~/x\"\n").unwrap();

        let out = Config::load_from(&dir, Some(&global)).unwrap();
        assert_eq!(out.config.host.as_deref(), Some("dot-config"));
        assert_eq!(out.config.remote_dir.as_deref(), Some("~/x"));
        assert!(out.warnings.is_empty());
        assert!(out.host_from_project);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_config_cannot_grant_itself_trust() {
        let dir = std::env::temp_dir().join(format!("cargo-remote-cfg3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        // Project file claims trust AND sets a host: both are dropped.
        std::fs::write(
            dir.join(".config/cargo-remote.toml"),
            "trust_project_config = true\nhost = \"evil\"\n",
        )
        .unwrap();

        let out = Config::load_from(&dir, None).unwrap();
        assert_eq!(out.config.host, None);
        assert_eq!(out.config.trust_project_config, None);
        assert!(!out.host_from_project);
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("host"), "{:?}", out.warnings);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_keys_still_flow_from_project_config() {
        let dir = std::env::temp_dir().join(format!("cargo-remote-cfg4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".cargo-remote.toml"),
            "toolchain = \"nightly\"\nenv = [\"A=1\"]\nsync = false\nexclude = [\"big\"]\n",
        )
        .unwrap();

        let out = Config::load_from(&dir, None).unwrap();
        assert_eq!(out.config.toolchain.as_deref(), Some("nightly"));
        assert_eq!(out.config.env.unwrap(), vec!["A=1".to_string()]);
        assert_eq!(out.config.sync, Some(false));
        assert_eq!(out.config.exclude.unwrap(), vec!["big".to_string()]);
        assert!(out.warnings.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
