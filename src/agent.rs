//! Remote sync agent protocol.
//!
//! The cargo-remote binary is uploaded to the remote once and runs there in
//! `__agent` mode, speaking a small framed protocol over the channel's
//! stdin/stdout. This replaces the external `rsync` binary: file deltas use
//! the rsync algorithm from [`crate::rsync`].
//!
//! Frame: `[u32 LE payload_len][u8 tag][payload]`.
//! Control payloads are JSON; bulk payloads (SIG/PUT/FETCHED) are
//! `[u32 json_len][header json][raw bytes]`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::progress::Progress;
use crate::rsync;
use crate::ssh::Ssh;

/// Bump when the wire format changes; the client re-uploads the agent then.
pub const PROTOCOL_VERSION: u32 = 1;

/// Full version string printed by `__agent --version` and compared on probe.
pub fn version_string() -> String {
    format!("{}-{}", PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"))
}

mod tag {
    pub const HELLO: u8 = 1;
    pub const ERROR: u8 = 2;
    pub const SYNC_REQ: u8 = 3;
    pub const LIST: u8 = 4;
    pub const SIG_REQ: u8 = 5;
    pub const SIG: u8 = 6;
    pub const PUT: u8 = 7;
    pub const DELETE: u8 = 8;
    pub const MKDIR: u8 = 9;
    pub const PLAN_DONE: u8 = 10;
    pub const DONE: u8 = 11;
    pub const FETCH_REQ: u8 = 12;
    pub const FETCH_SIG: u8 = 13;
    pub const FETCHED: u8 = 14;
    pub const QUIT: u8 = 15;
}

const KIND_FILE: u8 = 0;
const KIND_DIR: u8 = 1;
const KIND_LINK: u8 = 2;

#[derive(Serialize, Deserialize)]
struct Hello {
    proto: u32,
}

/// One filesystem entry, relative to the synced root.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileMeta {
    pub path: String,
    pub len: u64,
    pub mtime: i64,
    pub mode: u32,
    pub kind: u8,
    #[serde(default)]
    pub link: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SyncReq {
    root: String,
    delete: bool,
    excludes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct List {
    entries: Vec<FileMeta>,
}

#[derive(Serialize, Deserialize)]
struct Paths {
    paths: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct SigReq {
    paths: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SigHdr {
    path: String,
    #[serde(default)]
    block_len: u32,
}

#[derive(Serialize, Deserialize, Default)]
struct FileHdr {
    path: String,
    #[serde(default)]
    mode: u32,
    #[serde(default)]
    mtime: i64,
    #[serde(default)]
    kind: u8,
    #[serde(default)]
    link: Option<String>,
    /// Payload is a delta against the existing file (else full content).
    #[serde(default)]
    delta: bool,
    /// Block size the signature behind `delta` was made with (0 = default).
    #[serde(default)]
    block_len: u32,
    /// For FETCHED: false when the file does not exist on the sender.
    #[serde(default)]
    exists: bool,
}

fn hdr_block_len(h: u32) -> usize {
    if h == 0 { rsync::BLOCK_LEN } else { h as usize }
}

#[derive(Serialize, Deserialize)]
struct Done {
    files: u64,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct FetchReq {
    root: String,
    paths: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct ErrMsg {
    message: String,
}

// ---------------------------------------------------------------------------
// framing helpers
// ---------------------------------------------------------------------------

const MAX_FRAME: u32 = 512 * 1024 * 1024;

fn pack_hdr<T: Serialize>(hdr: &T, data: &[u8]) -> Result<Vec<u8>> {
    let j = serde_json::to_vec(hdr)?;
    let mut out = Vec::with_capacity(4 + j.len() + data.len());
    out.extend_from_slice(&(j.len() as u32).to_le_bytes());
    out.extend_from_slice(&j);
    out.extend_from_slice(data);
    Ok(out)
}

fn unpack_hdr<'a, T: serde::de::DeserializeOwned>(payload: &'a [u8]) -> Result<(T, &'a [u8])> {
    anyhow::ensure!(payload.len() >= 4, "short data frame");
    let jlen = u32::from_le_bytes(payload[..4].try_into().expect("4 bytes")) as usize;
    anyhow::ensure!(payload.len() >= 4 + jlen, "short data frame header");
    let hdr: T = serde_json::from_slice(&payload[4..4 + jlen])?;
    Ok((hdr, &payload[4 + jlen..]))
}

/// Frame I/O over a russh channel (client side).
pub struct ChannelIo {
    ch: crate::ssh::ClientChannel,
    buf: Vec<u8>,
    /// Captured remote stderr (tail), for error messages.
    stderr_tail: Vec<u8>,
}

impl ChannelIo {
    pub fn new(ch: crate::ssh::ClientChannel) -> Self {
        ChannelIo {
            ch,
            buf: Vec::new(),
            stderr_tail: Vec::new(),
        }
    }

    pub async fn read_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        loop {
            if self.buf.len() >= 5 {
                let len =
                    u32::from_le_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
                anyhow::ensure!(len as u32 <= MAX_FRAME, "frame too large: {len}");
                if self.buf.len() >= 5 + len {
                    let tag = self.buf[4];
                    let payload: Vec<u8> = self.buf[5..5 + len].to_vec();
                    self.buf.drain(..5 + len);
                    return Ok(Some((tag, payload)));
                }
            }
            match self.ch.wait().await {
                Some(russh::ChannelMsg::Data { data }) => self.buf.extend_from_slice(&data),
                Some(russh::ChannelMsg::ExtendedData { data, ext: 1 }) => {
                    self.stderr_tail.extend_from_slice(&data);
                    if self.stderr_tail.len() > 64 * 1024 {
                        let cut = self.stderr_tail.len() - 64 * 1024;
                        self.stderr_tail.drain(..cut);
                    }
                }
                Some(russh::ChannelMsg::Close) | None => {
                    if self.buf.is_empty() {
                        return Ok(None);
                    }
                    bail!(
                        "agent channel closed with {} unprocessed bytes; agent stderr tail: {}",
                        self.buf.len(),
                        String::from_utf8_lossy(&self.stderr_tail)
                    );
                }
                _ => {}
            }
        }
    }

    pub async fn write_frame(&mut self, tag: u8, payload: &[u8]) -> Result<()> {
        let mut v = Vec::with_capacity(5 + payload.len());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.push(tag);
        v.extend_from_slice(payload);
        self.ch.data_bytes(v).await?;
        Ok(())
    }

    pub async fn write_json<T: Serialize>(&mut self, tag: u8, v: &T) -> Result<()> {
        self.write_frame(tag, &serde_json::to_vec(v)?).await
    }
}

/// Frame I/O over process stdio (agent side).
struct StdIo {
    r: tokio::io::Stdin,
    w: tokio::io::Stdout,
}

impl StdIo {
    async fn read_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        use tokio::io::AsyncReadExt;
        let mut lenb = [0u8; 4];
        match self.r.read_exact(&mut lenb).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(lenb);
        anyhow::ensure!(len <= MAX_FRAME, "frame too large: {len}");
        let mut tag = [0u8; 1];
        self.r.read_exact(&mut tag).await?;
        let mut payload = vec![0u8; len as usize];
        self.r.read_exact(&mut payload).await?;
        Ok(Some((tag[0], payload)))
    }

    async fn write_frame(&mut self, tag: u8, payload: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut v = Vec::with_capacity(5 + payload.len());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.push(tag);
        v.extend_from_slice(payload);
        self.w.write_all(&v).await?;
        self.w.flush().await?;
        Ok(())
    }

    async fn write_json<T: Serialize>(&mut self, tag: u8, v: &T) -> Result<()> {
        self.write_frame(tag, &serde_json::to_vec(v)?).await
    }
}

// ---------------------------------------------------------------------------
// tree walking (shared semantics on both sides)
// ---------------------------------------------------------------------------

/// Root-anchored directory exclusion ("target" matches "target" and
/// "target/x" but not "src/target").
pub fn excluded(rel: &str, excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|e| rel == e || rel.starts_with(&format!("{e}/")))
}

fn entry_mode(md: &std::fs::Metadata, #[allow(unused)] is_dir: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if is_dir { 0o755 } else { 0o644 }
    }
}

/// Recursively list `root` (excluding `excludes`), sorted by path.
/// A missing root yields an empty list.
pub fn walk(root: &Path, excludes: &[String]) -> Result<Vec<FileMeta>> {
    let mut out = Vec::new();
    if root.is_dir() {
        walk_into(root, PathBuf::new().as_path(), excludes, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk_into(root: &Path, rel: &Path, excludes: &[String], out: &mut Vec<FileMeta>) -> Result<()> {
    for entry in std::fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let child_rel = rel.join(entry.file_name());
        let rel_str = child_rel.to_string_lossy().replace('\\', "/");
        if excluded(&rel_str, excludes) {
            continue;
        }
        let md = entry.metadata()?; // DirEntry::metadata does not follow symlinks
        let ft = md.file_type();
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if ft.is_dir() {
            out.push(FileMeta {
                path: rel_str.clone(),
                len: 0,
                mtime,
                mode: entry_mode(&md, true),
                kind: KIND_DIR,
                link: None,
            });
            walk_into(root, &child_rel, excludes, out)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(root.join(&child_rel))
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(FileMeta {
                path: rel_str,
                len: 0,
                mtime,
                mode: 0o777,
                kind: KIND_LINK,
                link: Some(target),
            });
        } else if ft.is_file() {
            out.push(FileMeta {
                path: rel_str,
                len: md.len(),
                mtime,
                mode: entry_mode(&md, false),
                kind: KIND_FILE,
                link: None,
            });
        }
    }
    Ok(())
}

fn same_entry(l: &FileMeta, r: &FileMeta) -> bool {
    if l.kind != r.kind {
        return false;
    }
    match l.kind {
        KIND_FILE => l.len == r.len && l.mtime == r.mtime,
        KIND_LINK => l.link == r.link,
        _ => true, // dirs: existence is enough
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(h) = std::env::home_dir() {
            return h;
        }
    } else if let Some(rest) = p.strip_prefix("~/")
        && let Some(h) = std::env::home_dir()
    {
        return h.join(rest);
    }
    PathBuf::from(p)
}

// ---------------------------------------------------------------------------
// client side
// ---------------------------------------------------------------------------

pub struct Agent {
    io: ChannelIo,
}

pub struct SyncStats {
    pub files: usize,
    pub sent_bytes: u64,
    pub full_bytes: u64,
}

impl Agent {
    /// Start the agent on the remote and check the protocol version.
    pub async fn start(ssh: &Ssh, agent_path_q: &str) -> Result<Agent> {
        let ch = ssh.open_exec_channel(&format!("{agent_path_q} __agent")).await?;
        let mut io = ChannelIo::new(ch);
        let (t, payload) = io
            .read_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent exited before HELLO"))?;
        if t == tag::ERROR {
            let e: ErrMsg = serde_json::from_slice(&payload)?;
            bail!("remote agent error: {}", e.message);
        }
        anyhow::ensure!(t == tag::HELLO, "agent: expected HELLO, got tag {t}");
        let hello: Hello = serde_json::from_slice(&payload)?;
        anyhow::ensure!(
            hello.proto == PROTOCOL_VERSION,
            "agent protocol mismatch: remote {} vs local {PROTOCOL_VERSION}",
            hello.proto
        );
        Ok(Agent { io })
    }

    pub async fn quit(mut self) -> Result<()> {
        let _ = self.io.write_frame(tag::QUIT, &[]).await;
        Ok(())
    }

    /// Sync `local_root` to `remote_root` on the agent's host, reporting
    /// live status to `progress`.
    pub async fn sync(
        &mut self,
        local_root: &Path,
        remote_root: &str,
        excludes: &[String],
        delete: bool,
        progress: &mut Progress,
    ) -> Result<SyncStats> {
        progress.status("scanning local files...");
        let local = walk(local_root, excludes)?;
        progress.status("fetching remote file list...");
        self.io
            .write_json(
                tag::SYNC_REQ,
                &SyncReq {
                    root: remote_root.to_string(),
                    delete,
                    excludes: excludes.to_vec(),
                },
            )
            .await?;
        let (t, payload) = self.expect(tag::LIST).await?;
        let _ = t;
        let list: List = serde_json::from_slice(&payload)?;

        let remote_map: std::collections::HashMap<&str, &FileMeta> = list
            .entries
            .iter()
            .map(|m| (m.path.as_str(), m))
            .collect();
        let local_map: std::collections::HashMap<&str, &FileMeta> =
            local.iter().map(|m| (m.path.as_str(), m)).collect();

        // Plan.
        let mut mkdirs = Vec::new();
        let mut sends: Vec<&FileMeta> = Vec::new();
        let mut sig_wanted: Vec<String> = Vec::new();
        for m in &local {
            match remote_map.get(m.path.as_str()) {
                None => {
                    if m.kind == KIND_DIR {
                        mkdirs.push(m.path.clone());
                    } else {
                        sends.push(m);
                    }
                }
                Some(r) if !same_entry(m, r) => {
                    sends.push(m);
                    if m.kind == KIND_FILE && r.kind == KIND_FILE && r.len > 0 {
                        sig_wanted.push(m.path.clone());
                    }
                }
                _ => {}
            }
        }
        let mut deletes: Vec<&FileMeta> = Vec::new();
        if delete {
            for r in &list.entries {
                if !local_map.contains_key(r.path.as_str()) {
                    deletes.push(r);
                }
            }
            // Deepest paths first, directories last so they can be removed.
            deletes.sort_by(|a, b| {
                b.path
                    .matches('/')
                    .count()
                    .cmp(&a.path.matches('/').count())
                    .then(b.path.cmp(&a.path))
            });
        }

        let total_files = sends.iter().filter(|m| m.kind == KIND_FILE).count() as u64;
        let total_bytes: u64 = sends
            .iter()
            .filter(|m| m.kind == KIND_FILE)
            .map(|m| m.len)
            .sum();
        progress.set_totals(Some(total_bytes), Some(total_files));

        // Execute the plan.
        if !mkdirs.is_empty() {
            self.io
                .write_json(tag::MKDIR, &Paths { paths: mkdirs })
                .await?;
        }
        if !sig_wanted.is_empty() {
            self.io
                .write_json(tag::SIG_REQ, &SigReq { paths: sig_wanted.clone() })
                .await?;
        }

        // Collect signatures (agent answers with exactly one SIG per path).
        // The remote reads and checksums each file, so this can take a
        // while on large trees: report it as its own phase.
        let mut sigs: std::collections::HashMap<String, (u32, Vec<rsync::BlockSig>)> =
            std::collections::HashMap::new();
        for (i, _) in sig_wanted.iter().enumerate() {
            progress.status(&format!(
                "collecting remote signatures: {}/{}",
                i + 1,
                sig_wanted.len()
            ));
            let (t, payload) = self.expect(tag::SIG).await?;
            let _ = t;
            let (hdr, data): (SigHdr, _) = unpack_hdr(&payload)?;
            if !data.is_empty() {
                sigs.insert(
                    hdr.path,
                    (hdr.block_len, rsync::sig_from_bytes(data)?),
                );
            }
        }
        progress.clear_status();

        let mut stats = SyncStats {
            files: 0,
            sent_bytes: 0,
            full_bytes: 0,
        };
        for m in &sends {
            let hdr = FileHdr {
                path: m.path.clone(),
                mode: m.mode,
                mtime: m.mtime,
                kind: m.kind,
                link: m.link.clone(),
                delta: false,
                block_len: 0,
                exists: true,
            };
            if m.kind != KIND_FILE {
                self.io
                    .write_frame(tag::PUT, &pack_hdr(&hdr, &[])?)
                    .await?;
                continue;
            }
            let data = std::fs::read(local_root.join(&m.path))
                .with_context(|| format!("read {}", m.path))?;
            stats.files += 1;
            stats.full_bytes += data.len() as u64;
            let payload = match sigs.get(&m.path) {
                Some((bl, sig)) => {
                    let d = rsync::delta(&data, sig, *bl as usize);
                    if d.len() < data.len() {
                        let hdr = FileHdr {
                            delta: true,
                            block_len: *bl,
                            ..hdr
                        };
                        pack_hdr(&hdr, &d)?
                    } else {
                        pack_hdr(&hdr, &data)?
                    }
                }
                None => pack_hdr(&hdr, &data)?,
            };
            stats.sent_bytes += payload.len() as u64;
            self.io.write_frame(tag::PUT, &payload).await?;
            progress.advance(data.len() as u64, payload.len() as u64);
            progress.inc_file();
        }

        if !deletes.is_empty() {
            self.io
                .write_json(
                    tag::DELETE,
                    &Paths {
                        paths: deletes.iter().map(|m| m.path.clone()).collect(),
                    },
                )
                .await?;
        }
        self.io.write_frame(tag::PLAN_DONE, &[]).await?;
        let (_, payload) = self.expect(tag::DONE).await?;
        let done: Done = serde_json::from_slice(&payload)?;
        let _ = done;
        Ok(stats)
    }

    /// Fetch `rel_paths` (relative to `remote_root`) into `dest_dir` (by
    /// basename), using deltas against existing local files, reporting
    /// live status to `progress`.
    /// Returns the basenames written.
    pub async fn fetch(
        &mut self,
        remote_root: &str,
        rel_paths: &[String],
        dest_dir: &Path,
        progress: &mut Progress,
    ) -> Result<Vec<String>> {
        self.io
            .write_json(
                tag::FETCH_REQ,
                &FetchReq {
                    root: remote_root.to_string(),
                    paths: rel_paths.to_vec(),
                },
            )
            .await?;
        // Send one signature per requested file (empty when we have no basis).
        for (i, rel) in rel_paths.iter().enumerate() {
            progress.status(&format!(
                "computing local signatures: {}/{}",
                i + 1,
                rel_paths.len()
            ));
            let local = dest_dir.join(basename(rel));
            let (sig, bl) = match std::fs::read(&local) {
                Ok(old) if !old.is_empty() => {
                    let bl = rsync::block_len_for(old.len() as u64);
                    (rsync::sig_to_bytes(&rsync::signature(&old, bl)), bl)
                }
                _ => (Vec::new(), rsync::BLOCK_LEN),
            };
            let hdr = SigHdr {
                path: rel.clone(),
                block_len: bl as u32,
            };
            self.io
                .write_frame(tag::FETCH_SIG, &pack_hdr(&hdr, &sig)?)
                .await?;
        }
        progress.clear_status();

        let mut written = Vec::new();
        for _ in rel_paths {
            let (_, payload) = self.expect(tag::FETCHED).await?;
            progress.add_wire(payload.len() as u64);
            progress.inc_file();
            let (hdr, data): (FileHdr, _) = unpack_hdr(&payload)?;
            if !hdr.exists {
                eprintln!("cargo-remote: {} vanished on remote, skipped", hdr.path);
                continue;
            }
            let dest = dest_dir.join(basename(&hdr.path));
            let content = if hdr.delta {
                let old = std::fs::read(&dest).unwrap_or_default();
                rsync::patch(&old, data, hdr_block_len(hdr.block_len))?
            } else {
                data.to_vec()
            };
            write_file_atomic(&dest, &content, hdr.mode, hdr.mtime)?;
            written.push(basename(&hdr.path).to_string());
        }
        let (_, payload) = self.expect(tag::DONE).await?;
        let _: Done = serde_json::from_slice(&payload)?;
        Ok(written)
    }

    async fn expect(&mut self, want: u8) -> Result<(u8, Vec<u8>)> {
        let (t, payload) = self
            .io
            .read_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent closed connection unexpectedly"))?;
        if t == tag::ERROR {
            let e: ErrMsg = serde_json::from_slice(&payload)?;
            bail!("remote agent error: {}", e.message);
        }
        anyhow::ensure!(t == want, "agent: expected tag {want}, got {t}");
        Ok((t, payload))
    }
}

// ---------------------------------------------------------------------------
// agent side (runs on the remote as `cargo-remote __agent`)
// ---------------------------------------------------------------------------

/// Serve the agent protocol on stdin/stdout. Only frames go to stdout.
pub async fn serve() -> Result<()> {
    let mut io = StdIo {
        r: tokio::io::stdin(),
        w: tokio::io::stdout(),
    };
    io.write_json(
        tag::HELLO,
        &Hello {
            proto: PROTOCOL_VERSION,
        },
    )
    .await?;
    loop {
        let Some((t, payload)) = io.read_frame().await? else {
            return Ok(());
        };
        let result = match t {
            tag::SYNC_REQ => {
                let req: SyncReq = serde_json::from_slice(&payload)?;
                serve_sync(&mut io, req).await
            }
            tag::FETCH_REQ => {
                let req: FetchReq = serde_json::from_slice(&payload)?;
                serve_fetch(&mut io, req).await
            }
            tag::QUIT => return Ok(()),
            _ => Err(anyhow::anyhow!("unexpected tag {t}")),
        };
        if let Err(e) = result {
            let _ = io
                .write_json(
                    tag::ERROR,
                    &ErrMsg {
                        message: format!("{e:#}"),
                    },
                )
                .await;
            return Err(e);
        }
    }
}

/// Write `content` to `dest` atomically, then apply mode and mtime.
fn write_file_atomic(dest: &Path, content: &[u8], mode: u32, mtime: i64) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("cargo-remote-part");
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    filetime::set_file_mtime(&tmp, filetime::FileTime::from_unix_time(mtime, 0))?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

async fn serve_sync(io: &mut StdIo, req: SyncReq) -> Result<()> {
    let root = expand_tilde(&req.root);
    let entries = walk(&root, &req.excludes)?;
    io.write_json(tag::LIST, &List { entries }).await?;

    let mut files = 0u64;
    let mut bytes = 0u64;
    loop {
        let Some((t, payload)) = io.read_frame().await? else {
            bail!("client disconnected mid-sync");
        };
        match t {
            tag::MKDIR => {
                let p: Paths = serde_json::from_slice(&payload)?;
                for d in &p.paths {
                    std::fs::create_dir_all(root.join(d))?;
                }
            }
            tag::DELETE => {
                let p: Paths = serde_json::from_slice(&payload)?;
                for d in &p.paths {
                    let path = root.join(d);
                    let md = std::fs::symlink_metadata(&path);
                    match md {
                        Ok(m) if m.is_dir() => {
                            let _ = std::fs::remove_dir(&path);
                        }
                        Ok(_) => {
                            let _ = std::fs::remove_file(&path);
                        }
                        Err(_) => {}
                    }
                }
            }
            tag::SIG_REQ => {
                let r: SigReq = serde_json::from_slice(&payload)?;
                for rel in &r.paths {
                    let path = root.join(rel);
                    let (sig, bl) = match std::fs::read(&path) {
                        Ok(data) if !data.is_empty() => {
                            let bl = rsync::block_len_for(data.len() as u64);
                            (rsync::sig_to_bytes(&rsync::signature(&data, bl)), bl)
                        }
                        _ => (Vec::new(), rsync::BLOCK_LEN),
                    };
                    let hdr = SigHdr {
                        path: rel.clone(),
                        block_len: bl as u32,
                    };
                    io.write_frame(tag::SIG, &pack_hdr(&hdr, &sig)?).await?;
                }
            }
            tag::PUT => {
                let (hdr, data): (FileHdr, _) = unpack_hdr(&payload)?;
                apply_put(&root, &hdr, data)?;
                if hdr.kind == KIND_FILE {
                    files += 1;
                    bytes += data.len() as u64;
                }
            }
            tag::PLAN_DONE => break,
            _ => bail!("unexpected tag {t} during sync"),
        }
    }
    io.write_json(tag::DONE, &Done { files, bytes }).await?;
    Ok(())
}

fn apply_put(root: &Path, hdr: &FileHdr, data: &[u8]) -> Result<()> {
    let dest = root.join(&hdr.path);
    match hdr.kind {
        KIND_DIR => {
            std::fs::create_dir_all(&dest)?;
        }
        KIND_LINK => {
            let target = hdr.link.clone().unwrap_or_default();
            let _ = std::fs::remove_file(&dest);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dest)?;
            #[cfg(not(unix))]
            bail!("symlinks are only supported on unix remotes");
        }
        _ => {
            let content = if hdr.delta {
                let old = std::fs::read(&dest).unwrap_or_default();
                rsync::patch(&old, data, hdr_block_len(hdr.block_len))?
            } else {
                data.to_vec()
            };
            write_file_atomic(&dest, &content, hdr.mode, hdr.mtime)?;
        }
    }
    Ok(())
}

async fn serve_fetch(io: &mut StdIo, req: FetchReq) -> Result<()> {
    let root = expand_tilde(&req.root);

    // Read one signature per requested path.
    let mut sigs: std::collections::HashMap<String, (u32, Vec<rsync::BlockSig>)> =
        std::collections::HashMap::new();
    for _ in 0..req.paths.len() {
        let Some((t, payload)) = io.read_frame().await? else {
            bail!("client disconnected during fetch");
        };
        anyhow::ensure!(t == tag::FETCH_SIG, "expected FETCH_SIG, got {t}");
        let (hdr, data): (SigHdr, _) = unpack_hdr(&payload)?;
        if !data.is_empty() {
            sigs.insert(hdr.path, (hdr.block_len, rsync::sig_from_bytes(data)?));
        }
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    for rel in &req.paths {
        let path = root.join(rel);
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let hdr = FileHdr {
                    path: rel.clone(),
                    exists: false,
                    ..Default::default()
                };
                io.write_frame(tag::FETCHED, &pack_hdr(&hdr, &[])?).await?;
            }
            Err(e) => return Err(e.into()),
            Ok(data) => {
                let md = std::fs::metadata(&path)?;
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let mut hdr = FileHdr {
                    path: rel.clone(),
                    mode: entry_mode(&md, false),
                    mtime,
                    kind: KIND_FILE,
                    link: None,
                    delta: false,
                    block_len: 0,
                    exists: true,
                };
                let payload = match sigs.get(rel) {
                    Some((bl, sig)) => {
                        let d = rsync::delta(&data, sig, *bl as usize);
                        if d.len() < data.len() {
                            hdr.delta = true;
                            hdr.block_len = *bl;
                            pack_hdr(&hdr, &d)?
                        } else {
                            pack_hdr(&hdr, &data)?
                        }
                    }
                    None => pack_hdr(&hdr, &data)?,
                };
                bytes += payload.len() as u64;
                files += 1;
                io.write_frame(tag::FETCHED, &payload).await?;
            }
        }
    }
    io.write_json(tag::DONE, &Done { files, bytes }).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(path: &str, len: u64, mtime: i64, kind: u8) -> FileMeta {
        FileMeta {
            path: path.to_string(),
            len,
            mtime,
            mode: 0o644,
            kind,
            link: None,
        }
    }

    #[test]
    fn excludes_are_root_anchored() {
        let ex = vec!["target".to_string(), ".git".to_string()];
        assert!(excluded("target", &ex));
        assert!(excluded("target/foo.rs", &ex));
        assert!(excluded(".git", &ex));
        assert!(!excluded("src/target", &ex));
        assert!(!excluded("src/main.rs", &ex));
    }

    #[test]
    fn walk_lists_files_dirs_and_links() {
        let dir = std::env::temp_dir().join(format!("cargo-remote-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(dir.join("src/deep/x.rs"), b"x").unwrap();
        std::fs::write(dir.join("target/ignored"), b"t").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/main.rs", dir.join("link.rs")).unwrap();

        let entries = walk(&dir, &["target".to_string()]).unwrap();
        let paths: Vec<&str> = entries.iter().map(|m| m.path.as_str()).collect();
        assert!(paths.contains(&"src"));
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/deep/x.rs"));
        assert!(!paths.contains(&"target"));
        assert!(!paths.contains(&"target/ignored"));
        #[cfg(unix)]
        {
            let link = entries.iter().find(|m| m.path == "link.rs").unwrap();
            assert_eq!(link.kind, KIND_LINK);
            assert_eq!(link.link.as_deref(), Some("src/main.rs"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hdr_pack_unpack_roundtrip() {
        let hdr = FileHdr {
            path: "a/b.rs".into(),
            mode: 0o644,
            mtime: 123,
            kind: KIND_FILE,
            link: None,
            delta: true,
            block_len: 4096,
            exists: true,
        };
        let payload = pack_hdr(&hdr, b"payload-bytes").unwrap();
        let (got, data): (FileHdr, _) = unpack_hdr(&payload).unwrap();
        assert_eq!(got.path, "a/b.rs");
        assert!(got.delta);
        assert_eq!(data, b"payload-bytes");
    }

    #[test]
    fn same_entry_compare() {
        let a = meta("f", 10, 100, KIND_FILE);
        assert!(same_entry(&a, &meta("f", 10, 100, KIND_FILE)));
        assert!(!same_entry(&a, &meta("f", 11, 100, KIND_FILE)));
        assert!(!same_entry(&a, &meta("f", 10, 101, KIND_FILE)));
        assert!(!same_entry(&a, &meta("f", 10, 100, KIND_DIR)));
    }
}
