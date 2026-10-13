//! Black-box security regression tests (Phase 0: shell-injection fixes).
//!
//! These run the real `cargo-remote` binary in --dry-run mode against
//! hostile inputs — repo-committed config files, shell-metachar project
//! directory names, crafted CLI flags — and assert that every payload byte
//! reaching a remote shell line is single-quoted (inert), or rejected
//! outright before any remote command is built.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cargo-remote"))
}

/// Isolated empty XDG_CONFIG_HOME so the developer's real global config
/// cannot leak into tests.
fn isolated_xdg() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cr-sec-xdg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str], cwd: &PathBuf) -> Output {
    run_env(args, cwd, &[])
}

fn run_env(args: &[&str], cwd: &PathBuf, env: &[(&str, &str)]) -> Output {
    let mut cmd = bin();
    cmd.args(args).current_dir(cwd);
    cmd.env("XDG_CONFIG_HOME", isolated_xdg());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run cargo-remote")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A minimal cargo project in a temp dir; name may contain shell payload.
struct TempProject(PathBuf);

impl TempProject {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "cr-sec-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut f = std::fs::File::create(root.join("Cargo.toml")).unwrap();
        write!(
            f,
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        )
        .unwrap();
        let mut f = std::fs::File::create(root.join("src/main.rs")).unwrap();
        writeln!(f, "fn main() {{}}").unwrap();
        TempProject(root)
    }

    fn write_config(&self, rel: &str, text: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `true` when every byte of each `payload` occurrence in `line` sits inside
/// a POSIX single-quoted region (with '\'' escape awareness).
fn payload_is_single_quoted(line: &str, payload: &str) -> bool {
    let bytes = line.as_bytes();
    let mut start = 0;
    while let Some(ix) = line[start..].find(payload) {
        let abs = start + ix;
        let end = abs + payload.len();
        let mut in_q = false;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\'' if !in_q => in_q = true,
                b'\'' if in_q && i + 3 < bytes.len() && &bytes[i..i + 4] == b"'\\''" => i += 3,
                b'\'' => in_q = false,
                _ => {}
            }
            if i >= abs && i < end && !in_q {
                return false;
            }
            i += 1;
        }
        start = end;
    }
    true
}

/// Assert every remote shell line is inert wrt every payload fragment.
/// (Only lines carrying remote commands — `ssh-exec HOST: <line>` — can
/// become input to a remote shell; plain display lines are not sinks.)
fn assert_all_inert(out: &Output, payloads: &[&str]) {
    let err = stderr(out);
    for line in err.lines() {
        if !line.contains("ssh-exec") {
            continue;
        }
        for p in payloads {
            assert!(
                payload_is_single_quoted(line, p),
                "payload {p:?} unquoted in remote line: {line:?}"
            );
        }
    }
}

/// Payload fragments we hunt for in output. The paths are canaries that
/// must never be created (dry-run never executes these lines; the quoting
/// assertions are the guard).
const FRAGS: &[&str] = &[
    "$(touch /tmp/cr-pwn",
    "`touch /tmp/cr-pwn",
    ";touch /tmp/cr-pwn",
    "; touch /tmp/cr-pwn",
];

// -------------------------------------------------------------------------
// Project directory name payloads (via tar extraction / malicious clone)
// -------------------------------------------------------------------------

#[test]
fn hostile_dir_name_cannot_inject() {
    let proj = TempProject::new("x$(touch /tmp/cr-pwn-dir1)");
    for args in [
        vec!["-r", "h", "-n", "sync"],
        vec!["-r", "h", "-n", "build"],
        vec!["-r", "h", "-n", "--transfer", "tar", "sync"],
        vec!["-r", "h", "-n", "clean"],
    ] {
        let out = run(&args, &proj.0);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        assert_all_inert(&out, FRAGS);
    }
    assert!(!PathBuf::from("/tmp/cr-pwn-dir1").exists());
}

#[test]
fn hostile_dir_name_backticks_and_semicolons() {
    let proj = TempProject::new("x`touch /tmp/cr-pwn-dir2`;touch /tmp/cr-pwn-dir2b");
    let out = run(&["-r", "h", "-n", "build"], &proj.0);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_all_inert(&out, FRAGS);
    assert!(!PathBuf::from("/tmp/cr-pwn-dir2").exists());
    assert!(!PathBuf::from("/tmp/cr-pwn-dir2b").exists());
}

// -------------------------------------------------------------------------
// Repo-committed config payloads (the clone-and-run attack)
// -------------------------------------------------------------------------

#[test]
fn hostile_config_remote_dir_is_quoted() {
    let proj = TempProject::new("cfg-rd");
    proj.write_config(
        ".cargo-remote.toml",
        "host = \"h\"\nremote_dir = \"~/b$(touch /tmp/cr-pwn-rd)\"\n",
    );
    for args in [
        vec!["-n", "sync"],
        vec!["-n", "--transfer", "tar", "sync"],
        vec!["-n", "build"],
    ] {
        let out = run(&args, &proj.0);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        assert_all_inert(&out, FRAGS);
    }
}

#[test]
fn hostile_config_env_is_quoted() {
    let proj = TempProject::new("cfg-env");
    proj.write_config(
        ".cargo-remote.toml",
        "host = \"h\"\nenv = [\"X=v$(touch /tmp/cr-pwn-env)\", \"Y=`touch /tmp/cr-pwn-env2`\", \"Z=a;touch /tmp/cr-pwn-env3\"]\n",
    );
    let out = run(&["-n", "build"], &proj.0);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_all_inert(&out, FRAGS);
    // And the values show up verbatim inside single quotes on the line.
    let err = stderr(&out);
    assert!(err.contains("X='v$(touch /tmp/cr-pwn-env)'"), "{err}");
}

#[test]
fn hostile_config_cargo_path_rejected() {
    let proj = TempProject::new("cfg-cp");
    for (value, desc) in [
        ("cargo; touch /tmp/cr-pwn-cp", "semicolon"),
        ("cargo$(touch /tmp/cr-pwn-cp)", "substitution"),
        ("cargo`touch /tmp/cr-pwn-cp`", "backticks"),
        ("cargo | sh", "pipe"),
    ] {
        proj.write_config(
            ".cargo-remote.toml",
            &format!("host = \"h\"\ncargo_path = \"{value}\"\n"),
        );
        let out = run(&["-n", "probe"], &proj.0);
        assert!(!out.status.success(), "{desc} accepted: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("invalid cargo_path"), "{desc}: {err}");
        let out = run(&["-n", "build"], &proj.0);
        assert!(!out.status.success(), "{desc} accepted in build");
    }
    assert!(!PathBuf::from("/tmp/cr-pwn-cp").exists());
}

#[test]
fn hostile_config_toolchain_rejected() {
    let proj = TempProject::new("cfg-tc");
    for value in ["nightly;id", "nightly$(id)", "nightly`id`", "x|y", "a b"] {
        proj.write_config(".cargo-remote.toml", &format!("host = \"h\"\ntoolchain = \"{value}\"\n"));
        let out = run(&["-n", "check"], &proj.0);
        assert!(!out.status.success(), "{value} accepted");
        assert!(stderr(&out).contains("invalid toolchain"), "{}", stderr(&out));
    }
}

#[test]
fn malformed_env_entries_rejected() {
    let proj = TempProject::new("cfg-env2");
    for (env_line, desc) in [
        ("env = [\"NOEQUALS\"]", "missing ="),
        ("env = [\"1BAD=x\"]", "leading digit"),
        ("env = [\"A-B=x\"]", "dash in key"),
        ("env = [\"=x\"]", "empty key"),
    ] {
        proj.write_config(".cargo-remote.toml", &format!("host = \"h\"\n{env_line}\n"));
        let out = run(&["-n", "build"], &proj.0);
        assert!(!out.status.success(), "{desc} accepted");
        let err = stderr(&out);
        assert!(err.contains("invalid env"), "{desc}: {err}");
    }
}

// -------------------------------------------------------------------------
// CLI flag payloads
// -------------------------------------------------------------------------

#[test]
fn cli_env_payload_is_quoted_but_key_validated() {
    let proj = TempProject::new("cli-env");
    // Value with substitution: quoted, line inert, run succeeds dry-run.
    let out = run(
        &["-r", "h", "-n", "--env", "A=v$(touch /tmp/cr-pwn-clienv)", "check"],
        &proj.0,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_all_inert(&out, FRAGS);
    // Bad key via CLI: rejected.
    let out = run(&["-r", "h", "-n", "--env", "1BAD=x", "check"], &proj.0);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("invalid env"), "{}", stderr(&out));
}

#[test]
fn cli_toolchain_payload_rejected() {
    let proj = TempProject::new("cli-tc");
    let out = run(
        &["-r", "h", "-n", "--toolchain", "nightly$(id)", "build"],
        &proj.0,
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("invalid toolchain"), "{}", stderr(&out));
}

#[test]
fn cli_profile_traversal_rejected() {
    let proj = TempProject::new("cli-prof");
    for p in ["../../out", "a/b", "x;id", "rel ease"] {
        let out = run(&["-r", "h", "-n", "build", "--profile", p], &proj.0);
        assert!(!out.status.success(), "--profile {p} accepted");
        assert!(stderr(&out).contains("invalid --profile"), "{}", stderr(&out));
    }
    // Sane profiles still accepted end to end.
    let out = run(&["-r", "h", "-n", "build", "--profile", "release-lto"], &proj.0);
    assert!(out.status.success(), "{}", stderr(&out));
}

// -------------------------------------------------------------------------
// RUSTFLAGS forwarding stays quoted (regression guard for existing code)
// -------------------------------------------------------------------------

#[test]
fn rustflags_payload_is_quoted() {
    let proj = TempProject::new("cfg-rf");
    let out = run_env(
        &["-r", "h", "-n", "build"],
        &proj.0,
        &[("RUSTFLAGS", "-Cfoo$(touch /tmp/cr-pwn-rf)")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("RUSTFLAGS='-Cfoo$(touch /tmp/cr-pwn-rf)'"), "{err}");
}

// -------------------------------------------------------------------------
// Legitimate configuration keeps working byte-for-byte
// -------------------------------------------------------------------------

#[test]
fn legit_config_line_is_unchanged() {
    let proj = TempProject::new("legit");
    proj.write_config(
        ".cargo-remote.toml",
        "host = \"h\"\nremote_dir = \"~/remote-builds\"\ncargo_path = \"$HOME/.cargo/bin/cargo\"\ntoolchain = \"nightly\"\nenv = [\"RUST_LOG=debug,x=y\", \"TOKEN=a b\"]\n",
    );
    let out = run(&["-n", "build", "--release"], &proj.0);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    let dir_key = format!("cr-sec-legit-{}", std::process::id());
    let want = format!(
        "cd \"$HOME\"/remote-builds/{dir_key} && RUST_LOG=debug,x=y TOKEN='a b' \
         \"$HOME\"/.cargo/bin/cargo +nightly build --release"
    );
    assert!(err.contains(&want), "{err}");
}
