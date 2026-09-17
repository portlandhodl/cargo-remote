use std::path::{Path, PathBuf};
use std::process::Command as ProcCommand;

/// The local cargo project (workspace) we are operating on.
#[derive(Debug)]
pub struct Project {
    /// Workspace root directory.
    pub root: PathBuf,
    /// Directory name used as the project key on the remote.
    pub name: String,
}

impl Project {
    /// Find the workspace root containing `dir` (via `cargo locate-project`,
    /// falling back to walking up the directory tree).
    pub fn discover(dir: PathBuf) -> anyhow::Result<Project> {
        let root = locate_workspace(&dir)
            .or_else(|| walk_up(&dir))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "could not find a Cargo.toml in {} or any parent directory",
                    dir.display()
                )
            })?;

        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "project".to_string());

        Ok(Project { root, name })
    }

    /// Local target/<profile> dir for copy-back.
    pub fn local_profile_dir(&self, profile: &str) -> PathBuf {
        self.root.join("target").join(profile)
    }
}

fn locate_workspace(dir: &Path) -> Option<PathBuf> {
    let out = ProcCommand::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let manifest = String::from_utf8(out.stdout).ok()?;
    let manifest = manifest.trim();
    if manifest.is_empty() {
        return None;
    }
    Path::new(manifest).parent().map(|p| p.to_path_buf())
}

fn walk_up(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join("Cargo.toml").is_file() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Cargo subcommands we know how to run remotely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CargoSubcommand {
    Build,
    Check,
    Clippy,
    Test,
    Run,
    Bench,
    Doc,
    Clean,
}

impl CargoSubcommand {
    pub fn as_str(&self) -> &'static str {
        match self {
            CargoSubcommand::Build => "build",
            CargoSubcommand::Check => "check",
            CargoSubcommand::Clippy => "clippy",
            CargoSubcommand::Test => "test",
            CargoSubcommand::Run => "run",
            CargoSubcommand::Bench => "bench",
            CargoSubcommand::Doc => "doc",
            CargoSubcommand::Clean => "clean",
        }
    }
}

/// Determine the cargo profile directory name from pass-through args:
/// `--release` -> "release", `--profile p` / `--profile=p` -> p, default "debug".
pub fn profile_from_args(args: &[String]) -> String {
    let mut profile = "debug".to_string();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            break; // everything after `--` belongs to the test/binary harness
        } else if a == "--release" {
            profile = "release".to_string();
        } else if a == "--profile" {
            if let Some(p) = args.get(i + 1) {
                profile = p.clone();
                i += 1;
            }
        } else if let Some(p) = a.strip_prefix("--profile=") {
            profile = p.to_string();
        }
        i += 1;
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_profile_is_debug() {
        assert_eq!(profile_from_args(&args(&[])), "debug");
        assert_eq!(profile_from_args(&args(&["--features", "x"])), "debug");
    }

    #[test]
    fn release_flag() {
        assert_eq!(profile_from_args(&args(&["--release"])), "release");
        assert_eq!(
            profile_from_args(&args(&["-j4", "--release", "--features", "y"])),
            "release"
        );
    }

    #[test]
    fn explicit_profile() {
        assert_eq!(
            profile_from_args(&args(&["--profile", "dev"])),
            "dev"
        );
        assert_eq!(
            profile_from_args(&args(&["--profile=custom"])),
            "custom"
        );
    }

    #[test]
    fn stops_at_double_dash() {
        assert_eq!(
            profile_from_args(&args(&["--", "--release"])),
            "debug"
        );
    }
}
