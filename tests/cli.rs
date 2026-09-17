use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cargo-remote"))
}

fn run(args: &[&str], cwd: Option<&PathBuf>) -> Output {
    let mut cmd = bin();
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.output().expect("failed to run cargo-remote")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Create a minimal cargo project in a temp dir.
struct TempProject(PathBuf);

impl TempProject {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cargo-remote-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut f = std::fs::File::create(root.join("Cargo.toml")).unwrap();
        write!(
            f,
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        )
        .unwrap();
        let mut f = std::fs::File::create(root.join("src/main.rs")).unwrap();
        writeln!(f, "fn main() {{}}").unwrap();
        TempProject(root)
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn hosts_runs() {
    let out = run(&["hosts"], None);
    assert!(out.status.success(), "hosts failed: {}", stderr(&out));
}

#[test]
fn cargo_style_invocation_strips_remote_token() {
    // `cargo remote hosts` executes us as `cargo-remote remote hosts`
    let direct = run(&["hosts"], None);
    let via_cargo = run(&["remote", "hosts"], None);
    assert!(via_cargo.status.success(), "{}", stderr(&via_cargo));
    assert_eq!(direct.stdout, via_cargo.stdout);
}

#[test]
fn dry_run_build_prints_sync_and_ssh() {
    let proj = TempProject::new("demo");
    let out = run(
        &["-r", "testhost", "--dry-run", "build", "--release"],
        Some(&proj.0),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("sync"), "missing sync step: {err}");
    assert!(err.contains("testhost"), "missing host: {err}");
    assert!(err.contains("cargo build --release"), "missing cargo cmd: {err}");
    assert!(err.contains("target/release"), "missing copy-back probe: {err}");
}

#[test]
fn dry_run_tar_gz_mode() {
    let proj = TempProject::new("demo2");
    let out = run(
        &["-r", "h", "-n", "--transfer", "tar-gz", "sync"],
        Some(&proj.0),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("gzip -c"), "missing local gzip: {err}");
    assert!(err.contains("gzip -dc"), "missing remote gunzip: {err}");
    assert!(err.contains("tar --exclude=./target"), "missing tar: {err}");
}

#[test]
fn missing_host_is_an_error() {
    let proj = TempProject::new("demo3");
    let out = run(&["build"], Some(&proj.0));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no remote host"), "{}", stderr(&out));
}

#[test]
fn cargo_remote_toml_provides_host() {
    let proj = TempProject::new("demo4");
    let mut f = std::fs::File::create(proj.0.join(".cargo-remote.toml")).unwrap();
    write!(f, "host = \"from-config\"\n").unwrap();
    let out = run(&["--dry-run", "sync"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("from-config"));
}

#[test]
fn unknown_subcommand_fails() {
    let out = run(&["frobnicate"], None);
    assert!(!out.status.success());
}

#[test]
fn no_sync_status_flag_accepted() {
    let proj = TempProject::new("demo5");
    let out = run(
        &["-r", "h", "-n", "--no-sync-status", "sync"],
        Some(&proj.0),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // Piped stderr is not a terminal, so no progress output either way.
    let err = stderr(&out);
    assert!(err.contains("sync"), "missing sync step: {err}");
}
