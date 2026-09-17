use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

use crate::agent::{Agent, PROTOCOL_VERSION, version_string};
use crate::project::{CargoSubcommand, Project, profile_from_args};
use crate::ssh::Ssh;
use crate::transfer::{Transfer, human};

/// Shared run switches.
pub struct Runner {
    pub dry_run: bool,
    pub verbose: bool,
}

impl Runner {
    /// Print a command/step line in verbose and dry-run modes.
    pub fn show_line(&self, line: &str) {
        if self.dry_run {
            eprintln!("(dry-run) + {line}");
        } else if self.verbose {
            eprintln!("+ {line}");
        }
    }
}

/// Fully resolved configuration for one invocation.
pub struct Context {
    pub runner: Runner,
    pub project: Project,
    pub host: String,
    pub remote_base: String,
    pub transfer: Transfer,
    pub copy_back: bool,
    pub sync: bool,
    pub exclude_git: bool,
    pub extra_exclude: Vec<String>,
    pub env: Vec<String>,
    pub toolchain: Option<String>,
    pub interactive: bool,
    pub cargo_path: String,
    /// None in dry-run mode (no connection is made).
    pub ssh: Option<Ssh>,
    agent: tokio::sync::OnceCell<Option<String>>,
}

impl Context {
    pub fn new(runner: Runner, project: Project, host: String, remote_base: String, transfer: Transfer, copy_back: bool, sync: bool, exclude_git: bool, extra_exclude: Vec<String>, env: Vec<String>, toolchain: Option<String>, interactive: bool, cargo_path: String, ssh: Option<Ssh>) -> Self {
        Context {
            runner,
            project,
            host,
            remote_base,
            transfer,
            copy_back,
            sync,
            exclude_git,
            extra_exclude,
            env,
            toolchain,
            interactive,
            cargo_path,
            ssh,
            agent: tokio::sync::OnceCell::new(),
        }
    }

    pub fn require_ssh(&self) -> Result<&Ssh> {
        self.ssh
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("internal error: no SSH connection (dry-run?)"))
    }

    /// Root-anchored directory exclusions shared by all transfer modes.
    pub fn excludes(&self) -> Vec<String> {
        let mut v = vec!["target".to_string()];
        if self.exclude_git {
            v.push(".git".into());
        }
        v.extend(self.extra_exclude.iter().cloned());
        v
    }

    /// Remote project dir with a leading `~` kept as-is.
    pub fn remote_dir(&self) -> String {
        format!("{}/{}", self.remote_base.trim_end_matches('/'), self.project.name)
    }

    /// Quote a remote path under the base dir for use in remote shell
    /// strings; a leading `~` becomes "$HOME" so it expands inside quotes.
    pub fn quote_remote(&self, path: &str) -> String {
        if path == "~" {
            "\"$HOME\"".to_string()
        } else if let Some(rest) = path.strip_prefix("~/") {
            format!("\"$HOME/{}\"", rest.replace('"', "\\\""))
        } else {
            format!("\"{}\"", path.replace('"', "\\\""))
        }
    }

    /// Remote project dir quoted for remote shell strings.
    pub fn remote_dir_q(&self) -> String {
        self.quote_remote(&self.remote_dir())
    }

    pub async fn sync(&self) -> Result<u8> {
        self.do_sync().await?;
        Ok(0)
    }

    async fn do_sync(&self) -> Result<()> {
        self.transfer.sync(self).await
    }

    pub async fn clean(&self) -> Result<u8> {
        let remote_cmd = self.remote_cargo_command(CargoSubcommand::Clean, &[]);
        let code = self.exec_remote(&remote_cmd).await?;
        Ok(code as u8)
    }

    pub async fn build(&self, cmd: CargoSubcommand, args: &[String]) -> Result<u8> {
        if self.sync {
            self.do_sync().await?;
        }

        let remote_cmd = self.remote_cargo_command(cmd, args);
        let code = self.exec_remote(&remote_cmd).await?;
        if code != 0 {
            return Ok(code as u8);
        }

        if cmd == CargoSubcommand::Build && self.copy_back {
            self.copy_back_binaries(args).await?;
        }
        Ok(0)
    }

    /// Run a remote command with terminal streaming (dry-run aware).
    async fn exec_remote(&self, remote_cmd: &str) -> Result<u32> {
        if self.runner.dry_run {
            self.runner
                .show_line(&format!("ssh-exec {}: {remote_cmd}", self.host));
            return Ok(0);
        }
        self.require_ssh()?
            .exec_term(remote_cmd, self.interactive)
            .await
    }

    fn remote_cargo_command(&self, cmd: CargoSubcommand, args: &[String]) -> String {
        let mut parts: Vec<String> = Vec::new();

        // Forward RUSTFLAGS if set locally, plus any --env KEY=VAL pairs.
        if let Ok(rf) = std::env::var("RUSTFLAGS")
            && !rf.is_empty()
        {
            parts.push(format!("RUSTFLAGS={}", shell_quote(&rf)));
        }
        for kv in &self.env {
            parts.push(kv.clone());
        }

        let cargo = if let Some(tc) = &self.toolchain {
            format!("{} +{}", self.cargo_path, tc)
        } else {
            self.cargo_path.clone()
        };

        let cargo_args = std::iter::once(cmd.as_str())
            .chain(args.iter().map(|s| s.as_str()))
            .collect::<Vec<_>>();

        let env = if parts.is_empty() {
            String::new()
        } else {
            format!("{} ", parts.join(" "))
        };

        format!(
            "cd {} && {}{} {}",
            self.remote_dir_q(),
            env,
            cargo,
            shell_join(cargo_args.into_iter())
        )
    }

    /// Make sure the remote agent (this binary, in __agent mode) is present
    /// and current. Returns the shell-quoted agent path, or None when the
    /// remote platform differs from local (agent can't run there).
    pub async fn ensure_agent(&self) -> Result<Option<String>> {
        self.agent
            .get_or_try_init(|| self.bootstrap_agent())
            .await
            .cloned()
    }

    async fn bootstrap_agent(&self) -> Result<Option<String>> {
        let ssh = self.require_ssh()?;

        // The agent is a copy of our own binary: platform must match.
        let (_, out) = ssh.exec_capture("uname -sm").await?;
        let uname = String::from_utf8_lossy(&out);
        let mut parts = uname.split_whitespace();
        let (os, arch) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let local_os = match std::env::consts::OS {
            "linux" => "Linux",
            "macos" => "Darwin",
            other => other,
        };
        if os != local_os || arch != std::env::consts::ARCH {
            if self.runner.verbose {
                eprintln!(
                    "cargo-remote: remote platform '{os} {arch}' differs from local \
                     '{local_os} {}' — sync agent disabled",
                    std::env::consts::ARCH
                );
            }
            return Ok(None);
        }

        let dir = format!("{}/.cargo-remote", self.remote_base.trim_end_matches('/'));
        let dir_q = self.quote_remote(&dir);
        let path_q = format!("{dir_q}/agent");
        let want = version_string();

        let (_, out) = ssh
            .exec_capture(&format!(
                "test -x {path_q} && {path_q} __agent --version 2>/dev/null || true"
            ))
            .await?;
        if String::from_utf8_lossy(&out).trim() == want {
            return Ok(Some(path_q));
        }

        // Upload ourselves.
        let exe = std::env::current_exe().context("locate own binary")?;
        let data = std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?;
        eprintln!(
            "cargo-remote: installing sync agent on {} ({}, protocol v{PROTOCOL_VERSION})",
            self.host,
            human(data.len() as u64)
        );
        let tmp_q = format!("{path_q}.tmp.{}", std::process::id());
        let cmd = format!("mkdir -p {dir_q} && cat > {tmp_q} && chmod 755 {tmp_q} && mv -f {tmp_q} {path_q}");
        let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        tokio::spawn(async move {
            for chunk in data.chunks(256 * 1024) {
                if tx.send(bytes::Bytes::copy_from_slice(chunk)).await.is_err() {
                    break;
                }
            }
        });
        let code = ssh.exec_send(&cmd, rx).await?;
        if code != 0 {
            bail!("agent upload failed with exit code {code}");
        }
        Ok(Some(path_q))
    }

    /// After a remote build: find produced binaries on the remote and pull
    /// them into the local target/<profile>/ dir (delta transfer when the
    /// agent is available). Also refreshes Cargo.lock if it changed.
    async fn copy_back_binaries(&self, args: &[String]) -> Result<()> {
        let profile = profile_from_args(args);
        if self.runner.dry_run {
            self.runner.show_line(&format!(
                "copy-back: fetch binaries from target/{profile}, refresh Cargo.lock"
            ));
            return Ok(());
        }
        let ssh = self.require_ssh()?;
        let find_cmd = format!(
            "cd {} && find target/{} -maxdepth 1 -type f \
             \\( -perm -u+x -o -name '*.so' -o -name '*.dylib' -o -name '*.dll' \\) \
             -print 2>/dev/null",
            self.remote_dir_q(),
            profile
        );
        let (code, out) = ssh.exec_capture(&find_cmd).await?;
        if code != 0 {
            bail!("remote find failed with exit code {code}");
        }
        let files: Vec<String> = String::from_utf8_lossy(&out)
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();

        let dest = self.project.local_profile_dir(&profile);
        if files.is_empty() {
            eprintln!("cargo-remote: no binaries found in remote target/{profile} (library crate?)");
        } else {
            std::fs::create_dir_all(&dest)?;
            let names = if let Some(agent_q) = self.ensure_agent().await? {
                let mut agent = Agent::start(ssh, &agent_q).await?;
                let got = agent.fetch(&self.remote_dir(), &files, &dest).await?;
                agent.quit().await?;
                got
            } else {
                // Fallback: plain full-file downloads.
                let mut got = Vec::new();
                for f in &files {
                    let name = f.rsplit('/').next().unwrap_or(f).to_string();
                    let src = format!("{}/{}", self.remote_dir(), f);
                    ssh.exec_download(&format!("cat {}", self.quote_remote(&src)), &dest.join(&name))
                        .await?;
                    got.push(name);
                }
                got
            };
            if self.runner.verbose {
                for n in &names {
                    eprintln!("copied back: {}", dest.join(n).display());
                }
            }
        }

        self.copy_back_lockfile().await?;
        Ok(())
    }

    async fn copy_back_lockfile(&self) -> Result<()> {
        let ssh = self.require_ssh()?;
        let remote_cmd = format!("cat {}/Cargo.lock 2>/dev/null || true", self.remote_dir_q());
        let (_, out) = ssh.exec_capture(&remote_cmd).await?;
        if out.is_empty() {
            return Ok(());
        }
        let local = self.project.root.join("Cargo.lock");
        let current = std::fs::read(&local).unwrap_or_default();
        if current != out {
            std::fs::write(&local, &out)?;
            eprintln!("cargo-remote: updated local Cargo.lock from remote");
        }
        Ok(())
    }
}

/// Shell-quote a string for POSIX sh (single-quote style).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Join argv words into a displayable/executable shell line.
pub fn shell_join<'a>(words: impl Iterator<Item = &'a str>) -> String {
    words.map(shell_quote).collect::<Vec<_>>().join(" ")
}

/// Write helper for tests.
#[allow(dead_code)]
pub(crate) fn write_file(path: &PathBuf, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    use std::io::Write;
    f.write_all(content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_simple() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a/b-c_d.txt"), "a/b-c_d.txt");
        assert_eq!(shell_quote("--features=foo,bar"), "--features=foo,bar");
    }

    #[test]
    fn quote_special() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a$b"), "'a$b'");
    }
}
