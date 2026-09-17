use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

use crate::agent::{Agent, PROTOCOL_VERSION, version_string};
use crate::progress::Progress;
use crate::project::{CargoSubcommand, Project, profile_from_args};
use crate::ssh::Ssh;
use crate::transfer::{Compressor, Transfer, human};

/// Shared run switches.
pub struct Runner {
    pub dry_run: bool,
    pub verbose: bool,
    /// Show the live sync status line (stderr is a TTY, not `--no-sync-status`).
    pub sync_status: bool,
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

/// Remote compatibility constraints from the config file, verified against
/// the live remote before syncing or building.
#[derive(Default)]
pub struct Requirements {
    /// Expected remote machine arch (`uname -m` spelling); "host" = the
    /// local machine's arch.
    pub arch: Option<String>,
    /// Minimum remote toolchain version, e.g. "1.85".
    pub min_rust_version: Option<String>,
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
    /// Configured remote constraints; filled in from the config file.
    pub requirements: Requirements,
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
            requirements: Requirements::default(),
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
        self.check_requirements(false).await?;
        self.do_sync().await?;
        Ok(0)
    }

    async fn do_sync(&self) -> Result<()> {
        self.transfer.sync(self).await
    }

    pub async fn clean(&self) -> Result<u8> {
        self.check_requirements(false).await?;
        let remote_cmd = self.remote_cargo_command(CargoSubcommand::Clean, &[]);
        let code = self.exec_remote(&remote_cmd).await?;
        Ok(code as u8)
    }

    pub async fn build(&self, cmd: CargoSubcommand, args: &[String]) -> Result<u8> {
        self.check_requirements(cmd == CargoSubcommand::Build && self.copy_back)
            .await?;
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

    /// Sync, then run an arbitrary cargo command line on the remote — cargo
    /// extensions like nextest: `argv` is `[subcommand, ...args]`. The
    /// extension binary (cargo-<subcommand>) must be installed on the
    /// remote; cargo's own error message says so if it is missing.
    pub async fn ext(&self, argv: &[String]) -> Result<u8> {
        self.check_requirements(false).await?;
        if self.sync {
            self.do_sync().await?;
        }
        let remote_cmd = self.remote_cargo_words(argv);
        let code = self.exec_remote(&remote_cmd).await?;
        Ok(code as u8)
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
        let words: Vec<String> = std::iter::once(cmd.as_str().to_string())
            .chain(args.iter().cloned())
            .collect();
        self.remote_cargo_words(&words)
    }

    /// Build the remote shell line for `cargo <words...>` (with env,
    /// toolchain shim, and cd into the remote project dir).
    fn remote_cargo_words(&self, words: &[String]) -> String {
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
            shell_join(words.iter().map(|s| s.as_str()))
        )
    }

    /// Verify the remote satisfies the configured `arch` and
    /// `min_rust_version` constraints, failing fast before any sync or
    /// build. With `warn_arch` (build + copy-back) and no explicit `arch`,
    /// a differing remote arch is a warning instead of an error.
    async fn check_requirements(&self, warn_arch: bool) -> Result<()> {
        if self.runner.dry_run {
            return Ok(());
        }
        let need_arch = self.requirements.arch.is_some() || warn_arch;
        let need_version = self.requirements.min_rust_version.is_some();
        if !need_arch && !need_version {
            return Ok(());
        }
        let ssh = self.require_ssh()?;

        if need_arch {
            let (_, out) = ssh.exec_capture("uname -m").await?;
            let remote = norm_arch(&String::from_utf8_lossy(&out));
            match &self.requirements.arch {
                Some(want) => {
                    let want = resolve_arch(want);
                    anyhow::ensure!(
                        remote == want,
                        "remote arch '{remote}' does not match required arch '{want}' \
                         (config `arch`); compiled outputs would not match"
                    );
                }
                None => {
                    let local = norm_arch(std::env::consts::ARCH);
                    if remote != local {
                        eprintln!(
                            "cargo-remote: warning: remote arch '{remote}' differs from local \
                             '{local}'; copied-back binaries will not run on this machine\n  \
                             set `arch = \"host\"` in the config to enforce a match, or \
                             `arch = \"{remote}\"` to silence this"
                        );
                    }
                }
            }
        }

        if let Some(min) = &self.requirements.min_rust_version {
            let cargo = match &self.toolchain {
                Some(tc) => format!("{} +{}", self.cargo_path, tc),
                None => self.cargo_path.clone(),
            };
            let (_, out) = ssh.exec_capture(&format!("{cargo} --version")).await?;
            let text = String::from_utf8_lossy(&out);
            let got = parse_version(&text).with_context(|| {
                format!("could not parse remote toolchain version from '{}'", text.trim())
            })?;
            let want = parse_version(min)
                .with_context(|| format!("invalid min_rust_version '{min}' in config"))?;
            anyhow::ensure!(
                got >= want,
                "remote toolchain is {}, but the config requires min_rust_version = \"{min}\"",
                text.trim()
            );
        }
        Ok(())
    }

    /// Probe the host for cargo-remote support: connection, auth, platform,
    /// toolchain, transfer tools, sync agent, remote dir and disk space.
    /// Prints one status line per check; returns 0 when nothing hard-failed.
    pub async fn probe(&self) -> Result<u8> {
        let mut failed = false;

        // What we resolved locally, before touching the network.
        let mut cfg_line = format!(
            "  config:    dir {}, transfer {}, cargo {}",
            self.remote_dir(),
            self.transfer.name(),
            self.cargo_path
        );
        if let Some(tc) = &self.toolchain {
            cfg_line.push_str(&format!(" +{tc}"));
        }
        eprintln!("cargo-remote: probing {}", self.host);
        eprintln!("{cfg_line}");
        let mut reqs = Vec::new();
        if let Some(a) = &self.requirements.arch {
            reqs.push(format!("arch {a}"));
        }
        if let Some(v) = &self.requirements.min_rust_version {
            reqs.push(format!("cargo >= {v}"));
        }
        if !reqs.is_empty() {
            eprintln!("  requires:  {}", reqs.join(", "));
        }
        let mut sent: Vec<String> = Vec::new();
        if let Ok(rf) = std::env::var("RUSTFLAGS")
            && !rf.is_empty()
        {
            sent.push(format!("RUSTFLAGS={rf}"));
        }
        sent.extend(self.env.iter().cloned());
        if !sent.is_empty() {
            eprintln!("  env sent:  {}", sent.join(" "));
        }

        // Connection (resolves ~/.ssh/config, verifies host key, authenticates).
        let ssh = match Ssh::connect(&self.host, self.interactive).await {
            Ok(s) => {
                let i = s.info();
                eprintln!("  connect:   ok ({}@{}:{})", i.user, i.host, i.port);
                s
            }
            Err(e) => {
                eprintln!("  connect:   FAIL ({e:#})");
                eprintln!("cargo-remote: {} is not usable", self.host);
                return Ok(1);
            }
        };

        // Platform: agent needs same OS+arch as local; copy-back needs arch.
        let local_os = match std::env::consts::OS {
            "linux" => "Linux",
            "macos" => "Darwin",
            other => other,
        };
        let local_arch = norm_arch(std::env::consts::ARCH);
        let mut agent_ok = false;
        let (code, out) = ssh.exec_capture("uname -sm").await?;
        if code == 0 {
            let uname = String::from_utf8_lossy(&out);
            let mut parts = uname.split_whitespace();
            let os = parts.next().unwrap_or("").to_string();
            let arch = norm_arch(parts.next().unwrap_or(""));
            agent_ok = os == local_os && arch == local_arch;
            match &self.requirements.arch {
                Some(want) if resolve_arch(want) != arch => {
                    eprintln!(
                        "  platform:  FAIL {os} {arch} (config requires arch {})",
                        resolve_arch(want)
                    );
                    failed = true;
                }
                None if arch != local_arch => {
                    eprintln!(
                        "  platform:  warn {os} {arch} (local is {local_arch}; copied-back binaries won't run here)"
                    );
                }
                _ => eprintln!("  platform:  ok {os} {arch}"),
            }
        } else {
            eprintln!("  platform:  FAIL (uname exited {code})");
            failed = true;
        }

        // Toolchain on the remote.
        let cargo = match &self.toolchain {
            Some(tc) => format!("{} +{}", self.cargo_path, tc),
            None => self.cargo_path.clone(),
        };
        let (code, out) = ssh.exec_capture(&format!("{cargo} --version")).await?;
        if code == 0 {
            let text = String::from_utf8_lossy(&out).trim().to_string();
            match parse_version(&text) {
                Ok(v) => {
                    let min = self
                        .requirements
                        .min_rust_version
                        .as_deref()
                        .map(parse_version)
                        .transpose()?;
                    match min {
                        Some(want) if v < want => {
                            eprintln!(
                                "  toolchain: FAIL {text} (config requires >= {})",
                                self.requirements.min_rust_version.as_deref().expect("checked")
                            );
                            failed = true;
                        }
                        _ => eprintln!("  toolchain: ok {text}"),
                    }
                }
                Err(_) => eprintln!("  toolchain: warn (unparsable version: {text})"),
            }
        } else {
            eprintln!("  toolchain: FAIL (no working cargo at {cargo})");
            failed = true;
        }

        // Transfer tools. Hard-fail only what the configured mode needs.
        let (_, out) = ssh
            .exec_capture("command -v tar gzip zstd 2>/dev/null")
            .await?;
        let have = path_basenames(&String::from_utf8_lossy(&out));
        let needs: &[&str] = match self.transfer {
            Transfer::Auto => &["tar", "gzip"], // cold start streams tar-gz
            Transfer::Tar {
                compressor: Compressor::None,
            } => &["tar"],
            Transfer::Tar {
                compressor: Compressor::Gzip,
            } => &["tar", "gzip"],
            Transfer::Tar {
                compressor: Compressor::Zstd,
            } => &["tar", "zstd"],
            Transfer::TarIncremental => &["tar"],
            Transfer::Delta => &[], // agent only; tar merely a fallback
        };
        let mut missing_req = Vec::new();
        let mut missing_opt = Vec::new();
        for t in ["tar", "gzip", "zstd"] {
            if have.iter().any(|h| h == t) {
                continue;
            }
            if needs.contains(&t) {
                missing_req.push(t);
            } else {
                missing_opt.push(t);
            }
        }
        if !missing_req.is_empty() {
            eprintln!(
                "  tools:     FAIL ({} needed by transfer mode '{}', not found on remote)",
                missing_req.join(", "),
                self.transfer.name()
            );
            failed = true;
        } else if !missing_opt.is_empty() {
            eprintln!(
                "  tools:     ok ({}; {} absent, only used by other transfer modes)",
                have.join(", "),
                missing_opt.join(", ")
            );
        } else {
            eprintln!("  tools:     ok (tar, gzip, zstd)");
        }

        // Sync agent: same platform as local -> can run; already installed?
        if agent_ok {
            let dir = format!("{}/.cargo-remote", self.remote_base.trim_end_matches('/'));
            let path_q = format!("{}/agent", self.quote_remote(&dir));
            let want = version_string();
            let (_, out) = ssh
                .exec_capture(&format!(
                    "test -x {path_q} && {path_q} __agent --version 2>/dev/null || true"
                ))
                .await?;
            let got = String::from_utf8_lossy(&out).trim().to_string();
            if got == want {
                eprintln!("  agent:     ok (installed, {got})");
            } else if got.is_empty() {
                eprintln!("  agent:     ok (not installed yet; will be uploaded on first sync)");
            } else {
                eprintln!("  agent:     warn (installed {got} != local {want}; will be re-uploaded)");
            }
        } else {
            eprintln!("  agent:     n/a (platform differs from local; tar-family transfers)");
        }

        // Remote dir creatable + free space for target/ dirs.
        let dir_q = self.remote_dir_q();
        let (code, out) = ssh
            .exec_capture(&format!("mkdir -p {dir_q} && df -h {dir_q}"))
            .await?;
        if code == 0 {
            let avail = df_avail(&String::from_utf8_lossy(&out)).unwrap_or_else(|| "?".into());
            eprintln!("  remote dir: ok ({}, {avail} available)", self.remote_dir());
        } else {
            eprintln!("  remote dir: FAIL (cannot create {})", self.remote_dir());
            failed = true;
        }

        if failed {
            eprintln!("cargo-remote: {} is NOT ready", self.host);
            Ok(1)
        } else {
            eprintln!("cargo-remote: {} is ready", self.host);
            Ok(0)
        }
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
        let total = data.len() as u64;
        eprintln!(
            "cargo-remote: installing sync agent on {} ({}, protocol v{PROTOCOL_VERSION})",
            self.host,
            human(total)
        );
        let tmp_q = format!("{path_q}.tmp.{}", std::process::id());
        let cmd = format!("mkdir -p {dir_q} && cat > {tmp_q} && chmod 755 {tmp_q} && mv -f {tmp_q} {path_q}");
        let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let mut progress = Progress::new("agent", &self.runner);
        progress.set_totals(Some(total), None);
        let send = tokio::spawn(async move {
            for chunk in data.chunks(256 * 1024) {
                if tx.send(bytes::Bytes::copy_from_slice(chunk)).await.is_err() {
                    break;
                }
                progress.advance(chunk.len() as u64, chunk.len() as u64);
            }
            progress
        });
        let code = ssh.exec_send(&cmd, rx).await?;
        let progress = send
            .await
            .map_err(|_| anyhow::anyhow!("agent upload task panicked"))?;
        if code != 0 {
            bail!("agent upload failed with exit code {code}");
        }
        progress.finish(&format!("uploaded {}", human(total)));
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
            let mut progress = Progress::new("copy-back", &self.runner);
            progress.set_totals(None, Some(files.len() as u64));
            let names = if let Some(agent_q) = self.ensure_agent().await? {
                let mut agent = Agent::start(ssh, &agent_q).await?;
                let got = agent
                    .fetch(&self.remote_dir(), &files, &dest, &mut progress)
                    .await?;
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
                    let size = std::fs::metadata(dest.join(&name))
                        .map(|m| m.len())
                        .unwrap_or(0);
                    progress.add_wire(size);
                    progress.inc_file();
                    got.push(name);
                }
                got
            };
            let received = progress.wire();
            progress.finish(&format!("{} files, {} received", names.len(), human(received)));
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

/// Basenames of `command -v`-style output (one absolute path per line).
fn path_basenames(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.trim().rsplit('/').next().map(|s| s.to_string()))
        .collect()
}

/// Available space from `df -h` output (Avail column of the data row).
fn df_avail(out: &str) -> Option<String> {
    out.lines()
        .last()?
        .split_whitespace()
        .nth(3)
        .map(|s| s.to_string())
}

/// Normalize an arch name to `uname -m` spelling (macOS says "arm64").
fn norm_arch(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    match s.as_str() {
        "arm64" => "aarch64".to_string(),
        _ => s,
    }
}

/// Resolve a configured `arch` value: "host" = the local machine's arch.
fn resolve_arch(cfg: &str) -> String {
    if cfg.trim().eq_ignore_ascii_case("host") {
        norm_arch(std::env::consts::ARCH)
    } else {
        norm_arch(cfg)
    }
}

/// Parse a version into comparable parts. Accepts plain ("1.85", "1.85.0")
/// and tool output ("cargo 1.85.0 (hash 2025-01-01)", "1.93.0-nightly").
fn parse_version(s: &str) -> Result<(u64, u64, u64)> {
    let tok = s
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .ok_or_else(|| anyhow::anyhow!("no version number found in '{s}'"))?;
    let mut parts = tok.split('.');
    let mut next = || -> Result<u64> {
        let Some(p) = parts.next() else { return Ok(0) };
        let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits
            .parse()
            .map_err(|_| anyhow::anyhow!("bad version component '{p}'"))
    };
    Ok((next()?, next()?, next()?))
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

    #[test]
    fn arch_normalization() {
        assert_eq!(norm_arch("x86_64"), "x86_64");
        assert_eq!(norm_arch("arm64"), "aarch64");
        assert_eq!(norm_arch(" aarch64\n"), "aarch64");
        assert_eq!(resolve_arch("host"), norm_arch(std::env::consts::ARCH));
        assert_eq!(resolve_arch("x86_64"), "x86_64");
        assert_eq!(resolve_arch("ARM64"), "aarch64");
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("1.85").unwrap(), (1, 85, 0));
        assert_eq!(parse_version("1.85.0").unwrap(), (1, 85, 0));
        assert_eq!(
            parse_version("cargo 1.85.0 (abc1234 2025-01-01)").unwrap(),
            (1, 85, 0)
        );
        assert_eq!(parse_version("cargo 1.93.0-nightly (abc)").unwrap(), (1, 93, 0));
        assert!(parse_version("garbage").is_err());
        assert!((1, 85, 0) >= (1, 84, 1) && (1, 85, 0) < (1, 85, 1));
    }

    #[test]
    fn basenames_from_command_v_output() {
        assert_eq!(
            path_basenames("/usr/bin/tar\n/bin/gzip\n/usr/local/bin/zstd\n"),
            vec!["tar", "gzip", "zstd"]
        );
        assert_eq!(path_basenames(""), Vec::<String>::new());
        assert_eq!(path_basenames("tar\n"), vec!["tar"]); // already a basename
    }

    #[test]
    fn df_avail_parses_data_row() {
        let out = "Filesystem      Size  Used Avail Use% Mounted on\n/dev/sda1       100G   40G   60G  40% /\n";
        assert_eq!(df_avail(out).as_deref(), Some("60G"));
        assert!(df_avail("").is_none());
    }
}
