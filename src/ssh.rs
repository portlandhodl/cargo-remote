//! Built-in SSH client (russh, pure Rust) — replaces the external `ssh` binary.
//!
//! Resolves hosts via ~/.ssh/config (russh-config), authenticates with
//! ssh-agent first and then identity files, and verifies server keys against
//! known_hosts (unknown keys require interactive confirmation, changed keys
//! are hard errors).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key, ssh_key};
use russh::{Channel, ChannelMsg};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// A connected, authenticated SSH session.
pub struct Ssh {
    handle: Handle<HostKeyHandler>,
    info: ConnInfo,
}

/// Resolved connection parameters, for status display.
pub struct ConnInfo {
    /// Resolved hostname or IP (after ~/.ssh/config).
    pub host: String,
    pub port: u16,
    pub user: String,
}

/// Channel type used for exec/agent sessions.
pub type ClientChannel = Channel<client::Msg>;

/// Bound on connect/handshake and channel setup.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on silence inside an exec_capture (metadata commands only).
const CAPTURE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// exec_capture output ceiling (Cargo.lock, find listings, probe output).
const MAX_CAPTURE: usize = 64 * 1024 * 1024;
/// exec_download size ceiling (copied-back binaries).
const MAX_DOWNLOAD: u64 = 4 * 1024 * 1024 * 1024;

/// What check_server_key decided, for error reporting after connect fails.
type Rejection = Arc<Mutex<Option<String>>>;

struct HostKeyHandler {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    strict: bool,
    interactive: bool,
    rejection: Rejection,
}

impl client::Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(_) => {
                *self.rejection.lock().unwrap() =
                    Some("server presented a certificate (unsupported)".into());
                return Ok(false);
            }
        };
        let checked = check_known_hosts_path(&self.host, self.port, &key, &self.known_hosts);
        match checked {
            Ok(true) => Ok(true),
            Ok(false) => self.unknown_key(&key).await,
            Err(keys_err) => {
                // A changed key is reported by russh as Error::KeyChanged.
                let msg = format!("{keys_err}");
                if msg.contains("changed") || msg.contains("KeyChanged") {
                    *self.rejection.lock().unwrap() = Some(format!(
                        "HOST KEY CHANGED for {}:{} — possible MITM attack!\n  {msg}\n  Remove the old key from {} if this is expected.",
                        self.host,
                        self.port,
                        self.known_hosts.display()
                    ));
                    return Ok(false);
                }
                // e.g. missing known_hosts file: treat as unknown.
                self.unknown_key(&key).await
            }
        }
    }
}

impl HostKeyHandler {
    async fn unknown_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, russh::Error> {
        let fp = key.fingerprint(HashAlg::Sha256);
        if self.strict || !self.interactive {
            *self.rejection.lock().unwrap() = Some(format!(
                "unknown host key for {}:{} ({fp}).\n  Run once interactively to accept it, or add it to {}.",
                self.host,
                self.port,
                self.known_hosts.display()
            ));
            return Ok(false);
        }
        eprint!(
            "cargo-remote: unknown host key for {}:{} ({}).\n  Trust it and add to {}? [y/N] ",
            self.host,
            self.port,
            fp,
            self.known_hosts.display()
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let accepted = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        })
        .await
        .unwrap_or(false);
        if accepted
            && let Err(e) = learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts)
        {
            eprintln!("cargo-remote: warning: could not record host key: {e}");
        }
        Ok(accepted)
    }
}

/// Expand a leading `~/` in a path from ssh config.
fn expand_home(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

impl Ssh {
    /// Resolve `alias` via ~/.ssh/config, connect, verify the host key,
    /// authenticate. When `interactive`, unknown host keys may be confirmed
    /// at a prompt.
    pub async fn connect(alias: &str, interactive: bool) -> Result<Ssh> {
        let cfg = russh_config::parse_home(alias)
            .unwrap_or_else(|_| russh_config::Config::default(alias));

        if cfg.host_config.proxy_jump.is_some() && cfg.host_config.proxy_command.is_none() {
            bail!(
                "{alias}: ProxyJump is not supported by the built-in SSH client yet.\n  \
                 Workaround: use an equivalent ProxyCommand (e.g. `ProxyCommand ssh -W %h:%p <jump>`)."
            );
        }

        let hostname = cfg.host().to_string();
        let port = cfg.port();
        let user = cfg.user();
        let known_hosts = cfg
            .host_config
            .user_known_hosts_file
            .as_ref()
            .map(|p| expand_home(p))
            .or_else(|| std::env::home_dir().map(|h| h.join(".ssh/known_hosts")))
            .ok_or_else(|| anyhow!("could not determine home directory"))?;
        let strict = cfg.host_config.strict_host_key_checking.unwrap_or(false);

        let rejection: Rejection = Arc::new(Mutex::new(None));
        let handler = HostKeyHandler {
            host: hostname.clone(),
            port,
            known_hosts,
            strict,
            interactive,
            rejection: rejection.clone(),
        };

        let config = Arc::new(client::Config {
            nodelay: true,
            keepalive_interval: Some(Duration::from_secs(15)),
            // A peer that stops talking entirely stalls forever otherwise.
            inactivity_timeout: Some(Duration::from_secs(600)),
            ..Default::default()
        });

        // Bound the connect+handshake so a stalling peer fails fast.
        let connect_fut = async {
            if cfg.host_config.proxy_command.is_some() {
                let stream = cfg
                    .stream()
                    .await
                    .map_err(|e| anyhow!("{alias}: proxy connect failed: {e}"))?;
                client::connect_stream(config, stream, handler)
                    .await
                    .map_err(anyhow::Error::from)
            } else {
                client::connect(config, (hostname.as_str(), port), handler)
                    .await
                    .map_err(anyhow::Error::from)
            }
        };
        let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connect_fut).await {
            Err(_) => bail!("{alias}: SSH connect to {hostname}:{port} timed out"),
            Ok(res) => res.map_err(|e| {
                if let Some(reason) = rejection.lock().unwrap().take() {
                    anyhow!("{reason}")
                } else {
                    anyhow!("{alias}: SSH connect to {hostname}:{port} failed: {e:#}")
                }
            })?,
        };

        authenticate(&mut handle, alias, &user, &cfg).await?;

        Ok(Ssh {
            handle,
            info: ConnInfo {
                host: hostname,
                port,
                user,
            },
        })
    }

    /// Resolved connection parameters (config alias already applied).
    pub fn info(&self) -> &ConnInfo {
        &self.info
    }

    /// Open a session channel running `cmd` (no pty).
    pub async fn open_exec_channel(&self, cmd: &str) -> Result<ClientChannel> {
        let open = self.handle.channel_open_session();
        let ch = tokio::time::timeout(CONNECT_TIMEOUT, open)
            .await
            .map_err(|_| anyhow!("channel open timed out"))?
            .context("open channel")?;
        tokio::time::timeout(CONNECT_TIMEOUT, ch.exec(true, cmd))
            .await
            .map_err(|_| anyhow!("remote exec request timed out"))?
            .context("exec remote command")?;
        Ok(ch)
    }

    /// Run `cmd` with output streamed to our terminal. With `pty`, a
    /// pseudo-terminal is requested (colors, progress bars) and our stdin is
    /// forwarded; without it the remote stdin is closed immediately.
    pub async fn exec_term(&self, cmd: &str, pty: bool) -> Result<u32> {
        let ch = self
            .handle
            .channel_open_session()
            .await
            .context("open channel")?;
        if pty {
            let (cols, rows) = terminal_size::terminal_size()
                .map(|(w, h)| (w.0 as u32, h.0 as u32))
                .unwrap_or((80, 24));
            ch.request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
                .await
                .context("request pty")?;
        }
        ch.exec(true, cmd).await.context("exec remote command")?;
        if !pty {
            ch.eof().await?;
        }

        let (mut rd, wr) = ch.split();
        let stdin_task = if pty {
            // Read stdin on a plain OS thread instead of tokio::io::stdin():
            // the latter uses an uncancellable blocking read that keeps the
            // runtime's blocking pool busy, so the process would hang on
            // shutdown whenever the terminal stdin never sends EOF.
            let (tx, mut rx) = mpsc::channel::<bytes::Bytes>(8);
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut stdin = std::io::stdin().lock();
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => {
                            if tx.blocking_send(bytes::Bytes::copy_from_slice(&buf[..n])).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            });
            Some(tokio::spawn(async move {
                while let Some(b) = rx.recv().await {
                    if wr.data_bytes(b).await.is_err() {
                        return;
                    }
                }
                let _ = wr.eof().await;
            }))
        } else {
            None
        };

        let mut out = tokio::io::stdout();
        let mut err = tokio::io::stderr();
        let mut code = None;
        while let Some(msg) = rd.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    out.write_all(&data).await?;
                    out.flush().await?;
                }
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    err.write_all(&data).await?;
                    err.flush().await?;
                }
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        if let Some(t) = stdin_task {
            t.abort();
        }
        Ok(code.unwrap_or(255))
    }

    /// Run `cmd`, capturing stdout; remote stderr goes to our stderr.
    /// Captured output is capped (metadata commands only) and a silent
    /// server aborts the wait.
    pub async fn exec_capture(&self, cmd: &str) -> Result<(u32, Vec<u8>)> {
        let mut ch = self.open_exec_channel(cmd).await?;
        ch.eof().await?;
        let mut stdout = Vec::new();
        let mut err = tokio::io::stderr();
        let mut code = None;
        loop {
            let msg = tokio::time::timeout(CAPTURE_IDLE_TIMEOUT, ch.wait()).await;
            match msg {
                Ok(Some(ChannelMsg::Data { data })) => {
                    stdout.extend_from_slice(&data);
                    if stdout.len() > MAX_CAPTURE {
                        bail!("remote output exceeded {} byte capture cap", MAX_CAPTURE);
                    }
                }
                Ok(Some(ChannelMsg::ExtendedData { data, ext: 1 })) => {
                    err.write_all(&data).await?;
                    err.flush().await?;
                }
                Ok(Some(ChannelMsg::ExitStatus { exit_status })) => code = Some(exit_status),
                Ok(Some(ChannelMsg::Close)) => break,
                Ok(None) => break,
                Err(_) => bail!("remote idle: no data for {}s", CAPTURE_IDLE_TIMEOUT.as_secs()),
                _ => {}
            }
        }
        Ok((code.unwrap_or(255), stdout))
    }

    /// Run `cmd` feeding it `input` as stdin; remote stderr goes to our
    /// stderr. Returns the exit code.
    pub async fn exec_send(&self, cmd: &str, mut input: mpsc::Receiver<bytes::Bytes>) -> Result<u32> {
        let mut ch = self.open_exec_channel(cmd).await?;
        let mut open = true;
        let mut code = None;
        let mut err = tokio::io::stderr();
        loop {
            tokio::select! {
                item = input.recv(), if open => match item {
                    Some(b) => ch.data_bytes(b).await.context("send to remote")?,
                    None => {
                        ch.eof().await.context("close remote stdin")?;
                        open = false;
                    }
                },
                msg = ch.wait() => match msg {
                    Some(ChannelMsg::Data { .. }) => {}
                    Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        err.write_all(&data).await?;
                        err.flush().await?;
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => code = Some(exit_status),
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        }
        Ok(code.unwrap_or(255))
    }

    /// Run `cmd` and stream its stdout into the local file `dest`
    /// (atomically, via an unguessable temp file, with a size ceiling).
    pub async fn exec_download(&self, cmd: &str, dest: &std::path::Path) -> Result<()> {
        let mut ch = self.open_exec_channel(cmd).await?;
        ch.eof().await?;
        let tmp = dest.with_file_name(format!(
            ".{}.dl-{}-{:x}",
            dest.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let result: Result<()> = async {
            let mut file = tokio::fs::File::create(&tmp)
                .await
                .with_context(|| format!("create {}", tmp.display()))?;
            let mut code = None;
            let mut total = 0u64;
            loop {
                let msg = tokio::time::timeout(CAPTURE_IDLE_TIMEOUT, ch.wait()).await;
                match msg {
                    Ok(Some(ChannelMsg::Data { data })) => {
                        total += data.len() as u64;
                        if total > MAX_DOWNLOAD {
                            bail!("download exceeded {MAX_DOWNLOAD} bytes");
                        }
                        file.write_all(&data).await?;
                    }
                    Ok(Some(ChannelMsg::ExitStatus { exit_status })) => code = Some(exit_status),
                    Ok(Some(ChannelMsg::Close)) => break,
                    Ok(None) => break,
                    Err(_) => bail!("remote idle: no data for {}s", CAPTURE_IDLE_TIMEOUT.as_secs()),
                    _ => {}
                }
            }
            file.flush().await?;
            drop(file);
            if code != Some(0) {
                bail!("remote command failed with code {}", code.unwrap_or(255));
            }
            // Only binary artifacts arrive here: make the result runnable,
            // never world-writable.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&tmp, dest)?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

}

/// Authenticate: ssh-agent identities first, then identity files from the
/// config (or the usual defaults) with an empty passphrase.
async fn authenticate(
    handle: &mut Handle<HostKeyHandler>,
    alias: &str,
    user: &str,
    cfg: &russh_config::Config,
) -> Result<()> {
    let rsa_hash = handle.best_supported_rsa_hash().await?.flatten();

    // 1) ssh-agent.
    if let Ok(mut agent) = AgentClient::connect_env().await
        && let Ok(identities) = agent.request_identities().await
    {
        for id in identities {
            let key = match id {
                AgentIdentity::PublicKey { key, .. } => key,
                AgentIdentity::Certificate { .. } => continue,
            };
            if let Ok(res) = handle
                .authenticate_publickey_with(user.to_string(), key, rsa_hash, &mut agent)
                .await
                && res.success()
            {
                return Ok(());
            }
        }
    }

    // 2) identity files.
    let from_cfg: Vec<PathBuf> = cfg
        .host_config
        .identity_file
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|p| expand_home(p))
        .collect();
    let candidates = if from_cfg.is_empty() {
        let mut v = Vec::new();
        if let Some(home) = std::env::home_dir() {
            for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                let p = home.join(".ssh").join(name);
                if p.is_file() {
                    v.push(p);
                }
            }
        }
        v
    } else {
        from_cfg
    };

    for path in candidates {
        let Ok(key) = load_secret_key(&path, None) else {
            continue; // missing or passphrase-protected
        };
        if let Ok(res) = handle
            .authenticate_publickey(
                user.to_string(),
                PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
            )
            .await
            && res.success()
        {
            return Ok(());
        }
    }

    bail!(
        "{alias}: authentication failed for {user}.\n  \
         No ssh-agent identity or passphrase-less key file worked.\n  \
         Hint: load your key with ssh-add (passphrase prompts are not supported yet)."
    )
}
