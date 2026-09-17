# cargo-remote

Compile Rust projects on a remote server, as if it were local.

Sources are synced incrementally, the cargo command runs on the server, and
built binaries are copied back into your local `target/` directory. Your
editor, `cargo test`, and one-off release builds all run at server speed while
you keep your local workflow.

```console
$ cargo remote -r my-server build --release
   Compiling mycrate v0.1.0 (/home/you/remote-builds/mycrate)
    Finished `release` profile [optimized] target(s) in 42.17s
$ ./target/release/mycrate --help        # the binary is back on your machine
```

## Highlights

- **Zero remote setup** — the server needs only a shell, `tar`, and cargo.
  No `rsync`, no daemons, no system packages.
- **No local SSH/rsync binaries either** — the SSH client is built in
  ([russh](https://github.com/Eugeny/russh), pure Rust), and delta sync is an
  in-process implementation of the rsync algorithm.
- **Fast repeat runs** — after the first sync, only *changed file blocks*
  cross the wire, and the remote build cache is reused. A no-change rebuild
  round-trips in a couple of seconds.
- **Uses your existing SSH setup** — host aliases from `~/.ssh/config`,
  verification against `~/.ssh/known_hosts`, ssh-agent authentication.
- **Feels local** — builds get a PTY when your terminal is interactive, so
  colors, progress bars and Ctrl-C behave as expected.
- **Live sync status** — while syncing you see bytes, percentage, throughput
  and ETA on a single updating line (only on a terminal; turn it off with
  `--no-sync-status`).

## Installation

From this repository:

```console
$ cargo install --path .
```

This provides the `cargo remote` subcommand (via the `cargo-remote` binary).

**Remote requirements:** a Linux or macOS server with a Rust toolchain.
The cargo path defaults to `$HOME/.cargo/bin/cargo` and is configurable.

## Quick start

Use the same host names as in your `~/.ssh/config`:

```console
$ cargo remote hosts                 # list hosts found in ~/.ssh/config
$ cargo remote -r my-server build    # sync, build remotely, copy binaries back
$ cargo remote -r my-server test     # sync, run the test suite on the server
```

Host resolution order: `-r/--remote <HOST>`, then `$CARGO_REMOTE_HOST`, then
`host = "..."` in `.cargo-remote.toml` in the workspace root.

Everything after the subcommand is passed through to cargo:

```console
$ cargo remote -r my-server build --release --features foo
$ cargo remote -r my-server test -- --nocapture
```

## Commands

| Command | What it does |
| --- | --- |
| `build` | Sync sources, run `cargo build` remotely, copy binaries back |
| `check` / `clippy` | Sync, then `cargo check` / `cargo clippy` remotely |
| `test` / `run` / `bench` / `doc` | Sync, then the matching cargo subcommand on the server |
| `clean` | `cargo clean` on the remote (clears the remote build cache) |
| `sync` | Only sync sources (pre-warm the remote cache, no build) |
| `hosts` | List host names found in `~/.ssh/config` |

Useful flags: `-n/--dry-run` shows what would run, `-v/--verbose` shows each
step, `--env KEY=VALUE` sets remote environment variables, `--toolchain NAME`
selects a rustup toolchain on the server, `--exclude-git` also skips `.git/`,
`--no-sync-status` disables the live sync progress display.

## How it works

1. **Sync.** On a warm remote, cargo-remote runs as a small agent on the
   server (the same binary, uploaded once per host on first use) and speaks a
   framed protocol over an SSH channel. Files are compared by size/mtime and
   transferred as rsync-style deltas: rolling-checksum block signatures with
   BLAKE2b strong hashes, so only changed blocks cross the wire. On a cold
   remote (or when the server's OS/arch differs from yours, so the agent
   binary can't run) it falls back to streaming a tar archive — no agent
   needed.
2. **Build.** The cargo command runs over an SSH channel with your
   environment (`RUSTFLAGS` is forwarded automatically). With an interactive
   terminal a PTY is allocated; stdin is forwarded.
3. **Copy back.** After `build`, binaries in `target/<profile>/` are pulled
   to your local `target/` (delta transfer when the agent is available), and
   `Cargo.lock` is refreshed if the remote build changed it.

### Transfer modes (`--transfer`)

| Mode | Behavior |
| --- | --- |
| `auto` (default) | tar-gz stream when the remote has no copy yet, delta sync afterwards |
| `rsync` | Built-in delta sync via the remote agent (no rsync binary involved) |
| `tar` / `tar-gz` / `tar-zstd` | Stream a full tar archive, optionally compressed |
| `tar-inc` | GNU tar incremental archives (local snapshot file) |

## Configuration

`.cargo-remote.toml` (workspace root):

```toml
host = "my-server"
cargo_path = "~/.cargo/bin/cargo"
```

Environment variables: `CARGO_REMOTE_HOST` (default host),
`CARGO_REMOTE_CARGO` (default cargo path), `RUSTFLAGS` (forwarded).

Remote project dirs live under `~/remote-builds/<project>` by default;
override with `--remote-dir`.

## SSH compatibility notes

- Supported from `~/.ssh/config`: `HostName`, `User`, `Port`, `IdentityFile`,
  `ProxyCommand`, `UserKnownHostsFile`, `StrictHostKeyChecking`.
- `ProxyJump` is not supported yet (you'll get a clear error; an equivalent
  `ProxyCommand ssh -W %h:%p <jump>` works).
- Authentication: ssh-agent identities or passphrase-less key files.
  Passphrase prompts are not implemented — use `ssh-add`.
- Host keys are verified against `known_hosts` (hashed entries supported).
  Unknown keys require interactive confirmation; a changed key aborts the
  connection.
- Agent forwarding is not available: remote builds cannot fetch private
  `git+ssh` dependencies.
- The delta-sync agent is the local binary copied to the server, so it only
  runs when local and remote OS/arch match (e.g. linux/x86_64 → linux/x86_64).
  Otherwise transfers automatically use tar.

## License

MIT — see [LICENSE](LICENSE).
