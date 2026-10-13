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
        let name = remote_safe_name(&name);

        Ok(Project { root, name })
    }

    /// Local target/<profile> dir for copy-back.
    pub fn local_profile_dir(&self, profile: &str) -> PathBuf {
        self.root.join("target").join(profile)
    }
}

/// Derive the remote project key from the local directory name. The name is
/// embedded in remote paths (and in the local sync snapshot file name), so
/// anything outside `[A-Za-z0-9._-]` is replaced with `_`; a short content
/// tag is appended to keep distinct directories distinct and detectable.
fn remote_safe_name(raw: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    if !raw.is_empty()
        && raw.chars().all(safe)
        && raw.chars().any(|c| c.is_ascii_alphanumeric())
    {
        return raw.to_string();
    }
    let mapped: String = raw
        .chars()
        .map(|c| if safe(c) { c } else { '_' })
        .take(40)
        .collect();
    let mapped = mapped.trim_matches(&['.', '_'][..]);
    let base = if mapped.is_empty() { "project" } else { mapped };
    format!("{base}-{}", short_tag(raw))
}

/// First 8 hex chars of BLAKE2b-512 of `s`; a deterministic disambiguation
/// tag for names that needed sanitizing.
fn short_tag(s: &str) -> String {
    use blake2::Digest;
    let digest = blake2::Blake2b512::digest(s.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
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
/// `--release` -> "release", `--profile p` / `--profile=p` -> p, default
/// "debug". The name lands in a remote shell `find` line and a local path,
/// so it is restricted to cargo's own profile-name character set.
pub fn profile_from_args(args: &[String]) -> anyhow::Result<String> {
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
    anyhow::ensure!(
        !profile.is_empty()
            && profile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
        "invalid --profile name '{profile}': expected [A-Za-z0-9_-]+"
    );
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_profile_is_debug() {
        assert_eq!(profile_from_args(&args(&[])).unwrap(), "debug");
        assert_eq!(profile_from_args(&args(&["--features", "x"])).unwrap(), "debug");
    }

    #[test]
    fn release_flag() {
        assert_eq!(profile_from_args(&args(&["--release"])).unwrap(), "release");
        assert_eq!(
            profile_from_args(&args(&["-j4", "--release", "--features", "y"])).unwrap(),
            "release"
        );
    }

    #[test]
    fn explicit_profile() {
        assert_eq!(profile_from_args(&args(&["--profile", "dev"])).unwrap(), "dev");
        assert_eq!(profile_from_args(&args(&["--profile=custom"])).unwrap(), "custom");
    }

    #[test]
    fn stops_at_double_dash() {
        // `--profile` after `--` belongs to the test harness, not cargo.
        assert_eq!(profile_from_args(&args(&["--", "--release"])).unwrap(), "debug");
        assert_eq!(
            profile_from_args(&args(&["--", "--profile", "x;id"])).unwrap(),
            "debug"
        );
    }

    #[test]
    fn rejects_shell_metachar_profiles() {
        for bad in [
            "release;id",
            "rel ease",
            "rel$(id)",
            "rel`id`",
            "a|b",
            "a&b",
            "a<b",
            "a>b",
            "a'b",
            "a\"b",
            "a\\b",
        ] {
            assert!(
                profile_from_args(&args(&["--profile", bad])).is_err(),
                "should reject {bad:?}"
            );
        }
        // `--profile=` form is validated too.
        assert!(profile_from_args(&args(&["--profile=x;id"])).is_err());
        // Empty name via `--profile ""`.
        assert!(profile_from_args(&args(&["--profile", ""])).is_err());
    }

    #[test]
    fn rejects_path_traversal_profiles() {
        for bad in ["../../out", "..", ".", "a/b", "a/./b"] {
            assert!(
                profile_from_args(&args(&["--profile", bad])).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn accepts_realistic_profiles() {
        for good in ["dev", "release", "release-lto", "ci_test-2", "BENCH3"] {
            assert_eq!(
                profile_from_args(&args(&["--profile", good])).unwrap(),
                good
            );
        }
    }

    #[test]
    fn remote_safe_name_passthrough() {
        for name in ["demo", "my-crate", "crate_2", "a.b.c", "x"] {
            assert_eq!(remote_safe_name(name), name, "{name} unchanged");
        }
    }

    #[test]
    fn remote_safe_name_maps_payloads() {
        for (raw, expect_prefix) in [
            ("a$(id)", "a__id-"),
            ("a`id`", "a_id-"),
            ("a b", "a_b"),
            ("a;id|b", "a_id_b"),
            ("proj\nname", "proj_name"),
            ("a/b\\c", "a_b_c"),
        ] {
            let got = remote_safe_name(raw);
            assert!(got.starts_with(expect_prefix), "{raw:?} -> {got}");
            assert!(
                got.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                "{got} must be shell/path-safe"
            );
        }
    }

    #[test]
    fn remote_safe_name_disambiguates() {
        // Two payloads mapping to the same base get different tags.
        let a = remote_safe_name("a$b");
        let b = remote_safe_name("a b");
        assert!(a.starts_with("a_b-") && b.starts_with("a_b-"));
        assert_ne!(a, b);
        // Deterministic across calls.
        assert_eq!(a, remote_safe_name("a$b"));
    }

    #[test]
    fn remote_safe_name_degenerate_inputs() {
        for raw in [".", "..", "...", "$(...)", "///"] {
            let got = remote_safe_name(raw);
            assert!(!got.is_empty());
            assert!(
                got.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                "{raw:?} -> {got} must be safe"
            );
            assert!(!got.starts_with('.'), "{got} must not be dotfile-ish");
        }
    }

    #[test]
    fn remote_safe_name_caps_length() {
        let long = format!("{}$({})", "a".repeat(200), "b".repeat(200));
        let got = remote_safe_name(&long);
        assert!(got.len() <= 49, "mapped(40) + '-' + tag(8): {}", got.len());
    }

    #[test]
    fn project_name_is_sanitized_on_discover() {
        // A workspace dir named with shell substitution must not keep its
        // raw name as the remote project key.
        let raw = format!("proj$(touch cr-pwn-{})", std::process::id());
        let root = std::env::temp_dir().join(&raw);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let p = Project::discover(root.clone()).unwrap();
        assert_ne!(p.name, raw);
        assert!(
            p.name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "name {:?} must be shell/path-safe",
            p.name
        );
        assert!(p.name.starts_with("proj__touch_cr-pwn-"), "{}", p.name);
        let _ = std::fs::remove_dir_all(&root);
    }
}
