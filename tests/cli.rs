use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cargo-remote"))
}

/// An empty dir used as XDG_CONFIG_HOME so the developer's real global
/// cargo-remote config (if any) cannot leak into tests.
fn isolated_xdg() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cargo-remote-test-xdg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str], cwd: Option<&PathBuf>) -> Output {
    run_env(args, cwd, &[])
}

fn run_env(args: &[&str], cwd: Option<&PathBuf>, env: &[(&str, &str)]) -> Output {
    let mut cmd = bin();
    cmd.args(args);
    cmd.env("XDG_CONFIG_HOME", isolated_xdg());
    for (k, v) in env {
        cmd.env(k, v);
    }
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
fn unknown_subcommand_is_passed_through_as_extension() {
    // Like cargo itself, unknown subcommands are treated as cargo extensions:
    // `cargo remote nextest run` runs `cargo nextest run` on the remote
    // (which errors there if cargo-nextest is not installed).
    let proj = TempProject::new("ext1");
    let out = run(
        &["-r", "h", "-n", "nextest", "run", "--profile", "fast"],
        Some(&proj.0),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("cargo nextest run --profile fast"),
        "missing passthrough command: {err}"
    );
    assert!(err.contains("sync"), "ext commands still sync first: {err}");
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

/// Write a config file into the project (rel path like
/// ".cargo-remote.toml" or ".config/cargo-remote.toml").
fn write_config(proj: &TempProject, rel: &str, text: &str) {
    let path = proj.0.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn dot_config_toml_provides_host() {
    let proj = TempProject::new("cfg1");
    write_config(&proj, ".config/cargo-remote.toml", "host = \"dot-config-host\"\n");
    let out = run(&["--dry-run", "sync"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("dot-config-host"), "{}", stderr(&out));
}

#[test]
fn root_config_beats_dot_config() {
    let proj = TempProject::new("cfg2");
    write_config(&proj, ".config/cargo-remote.toml", "host = \"dot-config-host\"\n");
    write_config(&proj, ".cargo-remote.toml", "host = \"root-host\"\n");
    let out = run(&["--dry-run", "sync"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("root-host"), "{err}");
    assert!(!err.contains("dot-config-host"), "{err}");
}

#[test]
fn global_config_provides_default_host() {
    let proj = TempProject::new("cfg3");
    let xdg = std::env::temp_dir().join(format!("cargo-remote-test-gxdg-{}", std::process::id()));
    std::fs::create_dir_all(xdg.join("cargo-remote")).unwrap();
    std::fs::write(
        xdg.join("cargo-remote/config.toml"),
        "host = \"global-host\"\n",
    )
    .unwrap();
    let out = run_env(&["--dry-run", "sync"], Some(&proj.0), &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())]);
    let _ = std::fs::remove_dir_all(&xdg);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("global-host"), "{}", stderr(&out));
}

#[test]
fn project_config_beats_global() {
    let proj = TempProject::new("cfg4");
    let xdg = std::env::temp_dir().join(format!("cargo-remote-test-gxdg2-{}", std::process::id()));
    std::fs::create_dir_all(xdg.join("cargo-remote")).unwrap();
    std::fs::write(
        xdg.join("cargo-remote/config.toml"),
        "host = \"global-host\"\n",
    )
    .unwrap();
    write_config(&proj, ".config/cargo-remote.toml", "host = \"project-host\"\n");
    let out = run_env(&["--dry-run", "sync"], Some(&proj.0), &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())]);
    let _ = std::fs::remove_dir_all(&xdg);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("project-host"), "{err}");
    assert!(!err.contains("global-host"), "{err}");
}

#[test]
fn config_transfer_mode_is_used() {
    let proj = TempProject::new("cfg5");
    write_config(
        &proj,
        ".config/cargo-remote.toml",
        "host = \"h\"\ntransfer = \"tar-gz\"\n",
    );
    let out = run(&["--dry-run", "sync"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("gzip -c"), "missing gzip pipeline: {err}");
}

#[test]
fn config_can_disable_sync() {
    let proj = TempProject::new("cfg6");
    write_config(
        &proj,
        ".config/cargo-remote.toml",
        "host = \"h\"\nsync = false\n",
    );
    let out = run(&["--dry-run", "build"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("cargo build"), "missing build: {err}");
    assert!(!err.contains("delta-sync"), "sync should be skipped: {err}");
}

#[test]
fn unknown_config_key_is_an_error() {
    let proj = TempProject::new("cfg7");
    write_config(&proj, ".config/cargo-remote.toml", "hostname = \"oops\"\n");
    let out = run(&["--dry-run", "sync"], Some(&proj.0));
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains(".config/cargo-remote.toml"), "{err}");
    assert!(err.contains("unknown field"), "{err}");
}

#[test]
fn arch_and_min_rust_version_are_accepted() {
    // Dry-run performs no network checks; this verifies parsing/plumbing.
    let proj = TempProject::new("cfg8");
    write_config(
        &proj,
        ".config/cargo-remote.toml",
        "host = \"h\"\narch = \"host\"\nmin_rust_version = \"1.85\"\n",
    );
    let out = run(&["--dry-run", "build"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn probe_unreachable_host_reports_failure() {
    // `.invalid` is reserved (RFC 6761): resolution always fails.
    let proj = TempProject::new("probe1");
    let out = run(&["-r", "nonexistent.invalid", "probe"], Some(&proj.0));
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("probing nonexistent.invalid"), "{err}");
    assert!(err.contains("connect:   FAIL"), "{err}");
    assert!(err.contains("not usable"), "{err}");
}

#[test]
fn cli_flag_beats_config_host() {
    let proj = TempProject::new("cfg9");
    write_config(&proj, ".config/cargo-remote.toml", "host = \"config-host\"\n");
    let out = run(&["-r", "flag-host", "--dry-run", "sync"], Some(&proj.0));
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("flag-host"), "{err}");
    assert!(!err.contains("config-host"), "{err}");
}

#[test]
fn env_var_beats_config_host() {
    let proj = TempProject::new("cfg10");
    write_config(&proj, ".config/cargo-remote.toml", "host = \"config-host\"\n");
    let out = run_env(
        &["--dry-run", "sync"],
        Some(&proj.0),
        &[("CARGO_REMOTE_HOST", "env-host")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("env-host"), "{err}");
    assert!(!err.contains("config-host"), "{err}");
}
