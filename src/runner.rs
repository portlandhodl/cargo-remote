use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::agent::{Agent, PROTOCOL_VERSION, checked_basename, display_safe, version_line};
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
    /// Apply the remote Cargo.lock locally after builds (--update-lockfile);
    /// otherwise a differing lockfile is saved to Cargo.lock.remote.
    pub update_lockfile: bool,
    pub sync: bool,
    pub exclude_git: bool,
    pub extra_exclude: Vec<String>,
    pub env: Vec<String>,
    /// RUSTFLAGS captured at startup (forwarded to the remote build).
    pub rustflags: Option<String>,
    pub toolchain: Option<String>,
    pub interactive: bool,
    pub cargo_path: String,
    /// None in dry-run mode (no connection is made).
    pub ssh: Option<Ssh>,
    /// Configured remote constraints; filled in from the config file.
    pub requirements: Requirements,
    /// Kept so the run lock lives until the Context is dropped.
    #[doc(hidden)]
    pub _run_lock: Option<crate::RunLock>,
    agent: tokio::sync::OnceCell<Option<String>>,
}

impl Context {
    pub fn new(runner: Runner, project: Project, host: String, remote_base: String, transfer: Transfer, copy_back: bool, update_lockfile: bool, sync: bool, exclude_git: bool, extra_exclude: Vec<String>, env: Vec<String>, rustflags: Option<String>, toolchain: Option<String>, interactive: bool, cargo_path: String, ssh: Option<Ssh>) -> Self {
        Context {
            runner,
            project,
            host,
            remote_base,
            transfer,
            copy_back,
            update_lockfile,
            sync,
            exclude_git,
            extra_exclude,
            env,
            rustflags,
            toolchain,
            interactive,
            cargo_path,
            ssh,
            requirements: Requirements::default(),
            _run_lock: None,
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

    /// Quote a remote path for use in remote shell strings.
    pub fn quote_remote(&self, path: &str) -> String {
        quote_remote_path(path)
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
        let remote_cmd = self.remote_cargo_command(CargoSubcommand::Clean, &[])?;
        let code = self.exec_remote(&remote_cmd).await?;
        Ok(code as u8)
    }

    pub async fn build(&self, cmd: CargoSubcommand, args: &[String]) -> Result<u8> {
        // Validate the copy-back profile up front so a bad --profile fails
        // before any sync or build work happens remotely.
        if cmd == CargoSubcommand::Build && self.copy_back {
            profile_from_args(args)?;
        }
        self.check_requirements(cmd == CargoSubcommand::Build && self.copy_back)
            .await?;
        if self.sync {
            self.do_sync().await?;
        }

        let remote_cmd = self.remote_cargo_command(cmd, args)?;
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
        let remote_cmd = self.remote_cargo_words(argv)?;
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

    fn remote_cargo_command(&self, cmd: CargoSubcommand, args: &[String]) -> Result<String> {
        let words: Vec<String> = std::iter::once(cmd.as_str().to_string())
            .chain(args.iter().cloned())
            .collect();
        self.remote_cargo_words(&words)
    }

    /// The remote cargo invocation (`cargo_path`, optional `+toolchain`),
    /// quoted for the remote shell.
    fn cargo_program(&self) -> String {
        let cargo = quote_remote_path(&self.cargo_path);
        match &self.toolchain {
            Some(tc) => format!("{cargo} +{}", shell_quote(tc)),
            None => cargo,
        }
    }

    /// Build the remote shell line for `cargo <words...>` (with env,
    /// toolchain shim, and cd into the remote project dir).
    fn remote_cargo_words(&self, words: &[String]) -> Result<String> {
        let mut parts: Vec<String> = Vec::new();

        // Forward RUSTFLAGS if set locally, plus any --env KEY=VAL pairs.
        if let Some(rf) = &self.rustflags {
            parts.push(format!("RUSTFLAGS={}", shell_quote(rf)));
        }
        push_env_prefix(&mut parts, &self.env)?;

        let cargo = self.cargo_program();

        let env = if parts.is_empty() {
            String::new()
        } else {
            format!("{} ", parts.join(" "))
        };

        Ok(format!(
            "cd {} && {}{} {}",
            self.remote_dir_q(),
            env,
            cargo,
            shell_join(words.iter().map(|s| s.as_str()))
        ))
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
            let cargo = self.cargo_program();
            let (_, out) = ssh.exec_capture(&format!("{cargo} --version")).await?;
            let text = String::from_utf8_lossy(&out);
            let got = parse_version(&text).with_context(|| {
                format!(
                    "could not parse remote toolchain version from '{}'",
                    display_safe(text.trim())
                )
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
        if let Some(rf) = &self.rustflags {
            sent.push(format!("RUSTFLAGS={rf}"));
        }
        sent.extend(self.env.iter().map(|kv| {
            if self.runner.verbose {
                kv.clone()
            } else {
                mask_env(kv)
            }
        }));
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
            let uname = display_safe(&String::from_utf8_lossy(&out));
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
        let cargo = self.cargo_program();
        let (code, out) = ssh.exec_capture(&format!("{cargo} --version")).await?;
        if code == 0 {
            let text = display_safe(String::from_utf8_lossy(&out).trim());
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
            let want = version_line()?;
            let (_, out) = ssh
                .exec_capture(&format!(
                    "test -x {path_q} && {path_q} __agent --version 2>/dev/null || true"
                ))
                .await?;
            let got = display_safe(String::from_utf8_lossy(&out).trim());
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
        let uname = display_safe(&String::from_utf8_lossy(&out));
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
        let want = version_line()?;

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
        // Unguessable staging name; the directory is pinned to 0700 so a
        // co-tenant cannot swap or snoop the installed agent.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_q = format!("{path_q}.tmp.{}-{nonce:x}", std::process::id());
        let cmd = format!(
            "mkdir -p {dir_q} && chmod 700 {dir_q} && cat > {tmp_q} && chmod 755 {tmp_q} && mv -f {tmp_q} {path_q}"
        );
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

        // uname matching is not enough: the binary must also start (older
        // remote glibc, noexec mounts, ... all refuse it). Verify, and let
        // the caller fall back to tar when the agent can't run.
        let (_, out) = ssh
            .exec_capture(&format!("{path_q} __agent --version 2>/dev/null"))
            .await?;
        if String::from_utf8_lossy(&out).trim() != want {
            eprintln!(
                "cargo-remote: sync agent does not run on {} (binary incompatible, e.g. newer glibc required); using tar transfer",
                self.host
            );
            return Ok(None);
        }
        Ok(Some(path_q))
    }

    /// After a remote build: find produced binaries on the remote and pull
    /// them into the local target/<profile>/ dir (delta transfer when the
    /// agent is available). Also refreshes Cargo.lock if it changed.
    async fn copy_back_binaries(&self, args: &[String]) -> Result<()> {
        let profile = profile_from_args(args)?;
        if self.runner.dry_run {
            self.runner.show_line(&format!(
                "copy-back: fetch binaries from target/{profile}, \
                 save a differing remote Cargo.lock to Cargo.lock.remote"
            ));
            return Ok(());
        }
        let ssh = self.require_ssh()?;
        let find_cmd = format!(
            "cd {} && find target/{} -maxdepth 1 -type f \
             \\( -perm -u+x -o -name '*.so' -o -name '*.dylib' -o -name '*.dll' \\) \
             -print0 2>/dev/null",
            self.remote_dir_q(),
            profile
        );
        let (code, out) = ssh.exec_capture(&find_cmd).await?;
        if code != 0 {
            bail!("remote find failed with exit code {code}");
        }
        // NUL-terminated: a filename containing a newline must not split
        // into phantom entries.
        let files: Vec<String> = out
            .split(|b| *b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .filter(|s| !s.trim().is_empty())
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
                    let name = checked_basename(f)?;
                    let src = format!("{}/{}", self.remote_dir(), f);
                    ssh.exec_download(&format!("cat {}", self.quote_remote(&src)), &dest.join(name))
                        .await?;
                    let size = std::fs::metadata(dest.join(name))
                        .map(|m| m.len())
                        .unwrap_or(0);
                    progress.add_wire(size);
                    progress.inc_file();
                    got.push(name.to_string());
                }
                got
            };
            let received = progress.wire();
            progress.finish(&format!("{} files, {} received", names.len(), human(received)));
            if self.runner.verbose {
                for n in &names {
                    eprintln!("copied back: {}", dest.join(display_safe(n)).display());
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
        apply_remote_lockfile(&self.project.root, &out, self.update_lockfile)
    }
}

/// `KEY=VALUE` → `KEY=***` for status output (env values are often secrets).
pub(crate) fn mask_env(kv: &str) -> String {
    match kv.split_once('=') {
        Some((key, _)) => format!("{key}=***"),
        None => kv.to_string(),
    }
}

/// Apply (or stage for review) a remote Cargo.lock that differs from local.
///
/// The remote build is only semi-trusted, so by default the differing
/// lockfile is written to `Cargo.lock.remote` for review; with `update`
/// (--update-lockfile) it replaces the local lockfile atomically.
fn apply_remote_lockfile(root: &Path, remote: &[u8], update: bool) -> Result<()> {
    if remote.is_empty() {
        return Ok(());
    }
    let local = root.join("Cargo.lock");
    if std::fs::read(&local).unwrap_or_default() == remote {
        return Ok(());
    }
    if update {
        let tmp = root.join(".cargo-remote-lockfile-tmp");
        std::fs::write(&tmp, remote)?;
        std::fs::rename(&tmp, &local)?;
        eprintln!("cargo-remote: updated local Cargo.lock from remote");
    } else {
        let side = root.join("Cargo.lock.remote");
        std::fs::write(&side, remote)?;
        eprintln!(
            "cargo-remote: remote Cargo.lock differs from local; saved to {} for review \
             (rerun with --update-lockfile to apply it)",
            side.display()
        );
    }
    Ok(())
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

/// Quote a path for a remote shell string, preserving HOME expansion:
/// `~`, `$HOME`, `~/x`, `$HOME/x` emit an unquoted `"$HOME"` prefix and a
/// single-quoted (expansion-inert) remainder. Everything else goes through
/// [`shell_quote`], so metacharacters (`$`, backticks, whitespace, quotes)
/// can never break out.
pub(crate) fn quote_remote_path(path: &str) -> String {
    if path == "~" || path == "$HOME" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("$HOME/"))
    {
        format!("\"$HOME\"/{}", shell_quote(rest))
    } else {
        shell_quote(path)
    }
}

/// Environment variable names must be POSIX shell identifiers.
pub(crate) fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reject malformed KEY=VALUE pairs from config/CLI before they ever reach
/// a remote command line.
pub(crate) fn validate_env_entries(env: &[String]) -> Result<()> {
    for kv in env {
        let (key, _) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid env entry '{kv}': expected KEY=VALUE"))?;
        anyhow::ensure!(
            is_valid_env_key(key),
            "invalid env variable name '{key}': expected [A-Za-z_][A-Za-z0-9_]*"
        );
    }
    Ok(())
}

/// rustup toolchain names ("stable", "nightly", "1.85.0", "nightly-2025-01-01").
pub(crate) fn is_valid_toolchain(s: &str) -> bool {
    !s.is_empty()
        && s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// `cargo_path` is emitted into the remote shell as the program to run, so
/// it must be a plain name/path over shell-inert characters, optionally
/// rooted at `~/` or `$HOME/`.
pub(crate) fn is_valid_cargo_path(s: &str) -> bool {
    let rest = s
        .strip_prefix("~/")
        .or_else(|| s.strip_prefix("$HOME/"))
        .unwrap_or(s);
    !rest.is_empty()
        && !rest.starts_with('-')
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '/' | '-'))
}

/// Push `KEY=VALUE` pairs onto a remote command's env prefix, validating
/// keys and single-quoting values so config/CLI-supplied data can never
/// execute on the remote shell.
fn push_env_prefix(parts: &mut Vec<String>, env: &[String]) -> Result<()> {
    for kv in env {
        let (key, value) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid env entry '{kv}': expected KEY=VALUE"))?;
        anyhow::ensure!(
            is_valid_env_key(key),
            "invalid env variable name '{key}': expected [A-Za-z_][A-Za-z0-9_]*"
        );
        parts.push(format!("{key}={}", shell_quote(value)));
    }
    Ok(())
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

    #[test]
    fn env_masking() {
        assert_eq!(mask_env("FOO=bar"), "FOO=***");
        assert_eq!(mask_env("TOKEN="), "TOKEN=***");
        assert_eq!(mask_env("A=b=c"), "A=***");
        assert_eq!(mask_env("NOEQUALS"), "NOEQUALS");
    }

    #[test]
    fn remote_lockfile_defaults_to_review_sidecar() {
        let dir = std::env::temp_dir().join(format!("cr-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.lock"), b"old").unwrap();

        // A differing remote lockfile goes to the sidecar, not Cargo.lock.
        apply_remote_lockfile(&dir, b"new", false).unwrap();
        assert_eq!(std::fs::read(dir.join("Cargo.lock")).unwrap(), b"old");
        assert_eq!(std::fs::read(dir.join("Cargo.lock.remote")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_lockfile_opt_in_applies_atomically() {
        let dir = std::env::temp_dir().join(format!("cr-lock2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.lock"), b"old").unwrap();

        apply_remote_lockfile(&dir, b"new", true).unwrap();
        assert_eq!(std::fs::read(dir.join("Cargo.lock")).unwrap(), b"new");
        assert!(!dir.join(".cargo-remote-lockfile-tmp").exists());
        assert!(!dir.join("Cargo.lock.remote").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_lockfile_noop_cases() {
        let dir = std::env::temp_dir().join(format!("cr-lock3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Empty remote output: nothing happens.
        apply_remote_lockfile(&dir, b"", false).unwrap();
        assert!(!dir.join("Cargo.lock").exists());
        assert!(!dir.join("Cargo.lock.remote").exists());
        // Identical content: nothing happens, in either mode.
        std::fs::write(dir.join("Cargo.lock"), b"same").unwrap();
        apply_remote_lockfile(&dir, b"same", true).unwrap();
        apply_remote_lockfile(&dir, b"same", false).unwrap();
        assert!(!dir.join("Cargo.lock.remote").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Phase-0 hardening: quoting & validators -----------------------

    /// Assert no shell metacharacter appears outside single quotes. A
    /// leading `"$HOME"` prefix is intentional (quoted, unquoted-var-safe)
    /// and skipped.
    fn assert_single_quote_inert(s: &str) {
        let s = s.strip_prefix("\"$HOME\"").unwrap_or(s);
        let b = s.as_bytes();
        let mut in_q = false;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'\'' if !in_q => in_q = true,
                b'\'' if in_q && i + 3 < b.len() && &b[i..i + 4] == b"'\\''" => i += 3,
                b'\'' => in_q = false,
                c if !in_q => assert!(
                    !b"$`|&;<>()\\\"' \t\n".contains(&c),
                    "metachar {:?} outside single quotes in {:?}",
                    c as char, s
                ),
                _ => {}
            }
            i += 1;
        }
        assert!(!in_q, "unbalanced single quotes in {s:?}");
    }

    const PAYLOADS: &[&str] = &[
        "$(touch \"$CR_CANARY\")",
        "`touch \"$CR_CANARY\"`",
        "; touch \"$CR_CANARY\"",
        "| touch \"$CR_CANARY\"",
        "&& touch \"$CR_CANARY\"",
        "|| touch \"$CR_CANARY\"; true",
        "> \"$CR_CANARY\"",
        "\"; touch \"$CR_CANARY\" #",
        "'; touch \"$CR_CANARY\" #",
        "\ntouch \"$CR_CANARY\"",
        " ${CR_CANARY:+touch \"$CR_CANARY\"} ",
    ];

    #[test]
    fn quote_remote_path_legit_forms() {
        assert_eq!(quote_remote_path("~"), "\"$HOME\"");
        assert_eq!(quote_remote_path("$HOME"), "\"$HOME\"");
        assert_eq!(
            quote_remote_path("~/remote-builds/proj"),
            "\"$HOME\"/remote-builds/proj"
        );
        assert_eq!(quote_remote_path("$HOME/x/y-2.z"), "\"$HOME\"/x/y-2.z");
        assert_eq!(quote_remote_path("/opt/builds/p"), "/opt/builds/p");
        assert_eq!(quote_remote_path("rel/path"), "rel/path");
    }

    #[test]
    fn quote_remote_path_metachars_are_single_quoted() {
        for p in PAYLOADS {
            assert_single_quote_inert(&quote_remote_path(&format!("~/x{p}")));
            assert_single_quote_inert(&quote_remote_path(&format!("/x{p}")));
            assert_single_quote_inert(&quote_remote_path(&format!("plain{p}")));
        }
        // Exact forms for the classic cases.
        assert_eq!(quote_remote_path("~/a$(id)"), "\"$HOME\"/'a$(id)'");
        assert_eq!(quote_remote_path("/x/`id`"), "'/x/`id`'");
        assert_eq!(
            quote_remote_path("/x/it'$(id)"),
            "'/x/it'\\''$(id)'"
        );
        assert_eq!(quote_remote_path("a b"), "'a b'");
    }

    #[test]
    fn env_key_rules() {
        for good in ["A", "RUST_LOG", "_", "X1", "A_B_C9"] {
            assert!(is_valid_env_key(good), "{good}");
        }
        for bad in ["", "1A", "A-B", "A B", "A$B", "A'B", "A.B", "A\nB"] {
            assert!(!is_valid_env_key(bad), "{bad}");
        }
    }

    #[test]
    fn env_entries_validation() {
        assert!(validate_env_entries(&[]).is_ok());
        assert!(
            validate_env_entries(&["A=b".into(), "RUST_LOG=".into(), "_X=$(id)".into()]).is_ok()
        );
        for bad in ["FOO", "", "=x", "1A=x", "A-B=x", "A\tB=x"] {
            assert!(validate_env_entries(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_prefix_quotes_values() {
        let mut parts = Vec::new();
        push_env_prefix(&mut parts, &["A=simple".into(), "B=two words".into()]).unwrap();
        assert_eq!(parts, ["A=simple", "B='two words'"]);
        let mut parts = Vec::new();
        push_env_prefix(&mut parts, &["X=v$(id)`id`;id".into()]).unwrap();
        assert_eq!(parts, ["X='v$(id)`id`;id'"]);
        // Values may be empty; keys that are not identifiers are rejected.
        let mut parts = Vec::new();
        push_env_prefix(&mut parts, &["E=".into()]).unwrap();
        assert_eq!(parts, ["E=''"]);
        for bad in ["NOEQUALS", "1X=y", "X-Y=z"] {
            let mut parts = Vec::new();
            assert!(push_env_prefix(&mut parts, &[bad.into()]).is_err(), "{bad}");
            assert!(parts.is_empty());
        }
    }

    #[test]
    fn toolchain_rules() {
        for good in ["stable", "nightly", "1.85.0", "nightly-2025-01-01", "1.85+components"] {
            assert!(is_valid_toolchain(good), "{good}");
        }
        for bad in [
            "", "nightly;id", "nightly$(id)", "`id`", "nightly x", "x|y", "x&y", "x\"y", "x'y",
            "x\ny", "x$",
        ] {
            assert!(!is_valid_toolchain(bad), "{bad}");
        }
    }

    #[test]
    fn cargo_path_rules() {
        for good in [
            "cargo",
            "/usr/bin/cargo",
            "$HOME/.cargo/bin/cargo",
            "~/.cargo/bin/cargo",
            "/opt/rust-1.85.0/bin/cargo",
            "cargo-beta",
        ] {
            assert!(is_valid_cargo_path(good), "{good}");
        }
        for bad in [
            "",
            "$HOME",
            "~/",
            "-x",
            "cargo;id",
            "cargo$(id)",
            "cargo x",
            "x|y",
            "x&y",
            "x\"y",
            "x'y",
            "x`y`",
            "x\ny",
            "$HOME/$(id)/cargo",
        ] {
            assert!(!is_valid_cargo_path(bad), "{bad}");
        }
    }

    fn test_context(
        env: Vec<String>,
        rustflags: Option<&str>,
        toolchain: Option<&str>,
        cargo_path: &str,
        remote_base: &str,
    ) -> Context {
        Context::new(
            Runner {
                dry_run: true,
                verbose: false,
                sync_status: false,
            },
            Project {
                root: PathBuf::from("/tmp/proj"),
                name: "demo".to_string(),
            },
            "h".to_string(),
            remote_base.to_string(),
            Transfer::Auto,
            false,
            false,
            true,
            false,
            vec![],
            env,
            rustflags.map(|s| s.to_string()),
            toolchain.map(|s| s.to_string()),
            false,
            cargo_path.to_string(),
            None,
        )
    }

    #[test]
    fn remote_cargo_line_construction() {
        // Valid config produces the expected, still-readable line.
        let ctx = test_context(
            vec!["FOO=a b".into(), "BAR=1".into()],
            Some("-Ctarget-cpu=native"),
            Some("nightly"),
            "$HOME/.cargo/bin/cargo",
            "~/remote-builds",
        );
        let line = ctx.remote_cargo_words(&["build".into(), "--release".into()]).unwrap();
        assert_eq!(
            line,
            "cd \"$HOME\"/remote-builds/demo && RUSTFLAGS=-Ctarget-cpu=native FOO='a b' BAR=1 \
             \"$HOME\"/.cargo/bin/cargo +nightly build --release"
        );
        // Malformed env entries are rejected even without main.rs validation.
        let ctx = test_context(vec!["NOEQUALS".into()], None, None, "cargo", "~/b");
        assert!(ctx.remote_cargo_words(&["check".into()]).is_err());

        // A fully poisoned context (validation bypassed) must still emit a
        // line whose every injected character is inside single quotes.
        let ctx = test_context(
            vec!["FOO=v$(id)`id`;id".into()],
            Some("-Cfoo$(id)"),
            Some("n$(id)"),
            "cargo; id",
            "~/b$(id)",
        );
        let line = ctx.remote_cargo_words(&["build".into()]).unwrap();
        eprintln!("constructed line: {line}");
        // The only legitimate occurrences of shell syntax in the line are
        // the word separators; the poisoned fragments must appear verbatim
        // inside a single-quoted region. Verify by checking each payload
        // substring sits inside quotes.
        for frag in ["$(id)", "`id`", ";id", "; id"] {
            let mut start = 0;
            while let Some(ix) = line[start..].find(frag) {
                let abs = start + ix;
                // Count unescaped single quotes before the fragment.
                let mut in_q = false;
                let bytes = line.as_bytes();
                let mut i = 0;
                while i < abs {
                    match bytes[i] {
                        b'\'' if !in_q => in_q = true,
                        b'\'' if in_q && i + 3 < bytes.len() && &bytes[i..i + 4] == b"'\\''" => i += 3,
                        b'\'' => in_q = false,
                        _ => {}
                    }
                    i += 1;
                }
                assert!(in_q, "{frag:?} outside single quotes in {line:?}");
                start = abs + frag.len();
            }
        }
    }

    // ---- Live POSIX sh canary tests (the source of truth for quoting) --

    #[cfg(unix)]
    mod sh_proof {
        use super::*;
        use std::sync::atomic::AtomicU64;

        static N: AtomicU64 = AtomicU64::new(0);

        /// A fresh nonexistent canary path for one test step.
        struct Canary(PathBuf);
        impl Canary {
            fn new(tag: &str) -> (Self, String) {
                let id = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!(
                    "cr-canary-{tag}-{}-{id}",
                    std::process::id()
                ));
                let _ = std::fs::remove_file(&p);
                let (c, s) = (Self(p.clone()), p.to_str().unwrap().to_string());
                (c, s)
            }
            fn fired(&self) -> bool {
                self.0.exists()
            }
        }

        fn sh(line: &str, envs: &[(&str, &str)]) -> std::process::ExitStatus {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(line);
            for (k, v) in envs {
                c.env(k, v);
            }
            c.status().expect("spawn sh")
        }

        #[test]
        fn quote_remote_path_blocks_substitution_in_sh() {
            let (canary, cpath) = Canary::new("q");
            for p in PAYLOADS {
                for base in ["~/x", "/x", "plain"] {
                    let q = quote_remote_path(&format!("{base}{p}"));
                    // `cd` will fail (dir does not exist) — but if quoting
                    // were broken, the payload would execute before that.
                    let status = sh(&format!("cd {q} && exit 3"), &[("CR_CANARY", &cpath)]);
                    assert!(!status.success());
                    assert!(
                        !canary.fired(),
                        "payload executed via {base}{p:?} -> {q}: {cpath}"
                    );
                }
            }
        }

        #[test]
        fn quote_remote_path_still_navigates_real_dirs() {
            // A dir with spaces under a fake HOME: cd must succeed.
            let home = std::env::temp_dir().join(format!("cr-home-{}", std::process::id()));
            let sub = home.join("my proj");
            std::fs::create_dir_all(&sub).unwrap();
            let q = quote_remote_path("~/my proj");
            let status = sh(
                &format!("cd {q} && touch ok"),
                &[("HOME", home.to_str().unwrap())],
            );
            assert!(status.success(), "cd {q} failed");
            assert!(sub.join("ok").exists());
            let _ = std::fs::remove_dir_all(&home);
        }

        #[test]
        fn remote_cargo_line_blocks_all_poisoned_fields_in_sh() {
            let (c0, p0) = Canary::new("r");
            let (c1, p1) = Canary::new("a");
            let (c2, p2) = Canary::new("b");
            let (c3, p3) = Canary::new("c");
            let (c4, p4) = Canary::new("d");
            let ctx = test_context(
                vec![
                    format!("FOO=v$(touch \"$CR1\")"),
                    format!("BAR=`touch \"$CR2\"`"),
                ],
                Some("-Cfoo$(touch \"$CR0\")"),
                Some("n$(touch \"$CR3\")"),
                "cargo; touch \"$CR4\"",
                "~/b$(touch \"$CR1\")",
            );
            let line = ctx.remote_cargo_words(&["build".into()]).unwrap();
            let status = sh(
                &line,
                &[
                    ("CR0", &p0),
                    ("CR1", &p1),
                    ("CR2", &p2),
                    ("CR3", &p3),
                    ("CR4", &p4),
                ],
            );
            // The line may fail (cd into nonexistent dir) but must never
            // have executed any payload.
            let _ = status;
            for (c, p) in [(&c0, &p0), (&c1, &p1), (&c2, &p2), (&c3, &p3), (&c4, &p4)] {
                assert!(!c.fired(), "payload executed from line {line:?} -> {p}");
            }
        }

        #[test]
        fn env_prefix_preserves_values_byte_for_byte() {
            let (out, out_path) = Canary::new("out");
            let (canary, can_path) = Canary::new("e");
            let value = "v$(touch \"$CR_CANARY\")`id`'q \"w\" \t tab \n nl $HOME ~ */;|&<>";
            let mut parts = Vec::new();
            push_env_prefix(&mut parts, &[format!("FOO={value}")]).unwrap();
            let prefix = format!("{} ", parts.join(" "));
            // Write the value back out from inside sh; it must round-trip
            // exactly and never execute.
            let line = format!("{prefix}sh -c 'printf %s \"$FOO\" > \"$CR_OUT\"'");
            let status = sh(&line, &[("CR_OUT", &out_path), ("CR_CANARY", &can_path)]);
            assert!(status.success(), "line failed: {line}");
            assert!(!canary.fired(), "value payload executed: {line}");
            assert_eq!(
                std::fs::read_to_string(&out.0).unwrap(),
                value,
                "value did not round-trip"
            );
        }
    }
}
