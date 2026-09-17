# Command reference

All commands accept the shared options (`-r/--remote`, `--remote-dir`,
`--transfer`, `--no-sync`, `--exclude-git`, `--exclude`, `--env`,
`--toolchain`, `-n/--dry-run`, `-v/--verbose`, `--no-sync-status`,
`--copy-back` / `--no-copy-back`), before or after the subcommand. Anything
after the subcommand that cargo-remote doesn't recognize is passed through
to the remote cargo.

## build

Sync sources, run `cargo build` on the remote, then copy the produced
binaries back into the local `target/<profile>/` directory (delta transfer
when the agent is available). Also refreshes the local `Cargo.lock` if the
remote build changed it.

```console
$ cargo remote build --release
cargo-remote: host my-server (you@10.0.0.2:22), dir ~/remote-builds/mycrate, transfer auto, cargo $HOME/.cargo/bin/cargo, requires arch host, cargo >= 1.85
cargo-remote: sync: 3 files changed, 4.1 KiB sent (210.2 KiB full size) in 0.4s (10.2 KiB/s)
   Compiling mycrate v0.1.0 (/home/you/remote-builds/mycrate)
    Finished `release` profile [optimized] target(s) in 42.17s
cargo-remote: copy-back: 1 files, 4.2 MiB received in 0.3s (14.0 MiB/s)
```

The first `cargo-remote: host ...` line always announces the effective
configuration before anything is transferred: resolved address, remote dir,
transfer mode, remote cargo, and any configured constraints. A second line
lists the exact environment variables sent to the remote build (only shown
when there are any). Copy-back can be disabled with `--no-copy-back` or
`copy_back = false` in the config.

## check / clippy / test / run / bench / doc

Sync, then run the matching cargo subcommand on the remote. Tests execute
on the server; `run` executes the binary on the server. With an interactive
terminal these get a PTY: colors, progress and Ctrl-C behave as if local.

## clean

Run `cargo clean` on the remote (clears the remote build cache).

## sync

Only sync sources — pre-warm the remote without building, e.g. before going
offline.

## hosts

Print the host aliases found in `~/.ssh/config` (candidates for `-r` or the
`host` config key). This is the only command that writes to stdout.

## probe

Test a host for cargo-remote support. Resolves the effective configuration,
then checks the live host, one line per check:

- **connect** — ssh config resolution, TCP, handshake, host-key
  verification against `known_hosts`, authentication (agent or key files)
- **platform** — remote OS/arch vs local; hard-fails on a configured `arch`
  mismatch, warns when copy-back binaries wouldn't run locally
- **toolchain** — remote `cargo --version` (respecting `toolchain`);
  hard-fails when older than a configured `min_rust_version`
- **tools** — `tar`/`gzip`/`zstd` presence; hard-fails only what the
  configured `transfer` mode needs
- **agent** — whether the sync agent can run (same OS/arch) and is
  installed and current
- **remote dir** — creatable and free disk space

```console
$ cargo remote probe
cargo-remote: probing anchorwatch-dev
  config:    dir ~/remote-builds/coastline, transfer auto, cargo $HOME/.cargo/bin/cargo
  requires:  arch host, cargo >= 1.96
  connect:   ok (qrsnap@10.10.0.222:22)
  platform:  ok Linux x86_64
  toolchain: ok cargo 1.96.0 (30a34c682 2026-05-25)
  tools:     ok (tar, gzip, zstd)
  agent:     ok (installed, 1-0.1.0)
  remote dir: ok (~/remote-builds/coastline, 90G available)
cargo-remote: anchorwatch-dev is ready
```

Exit code is 0 when the host is ready, 1 when any check hard-failed
(warnings alone still pass), 2 for local errors (no host configured,
invalid config file).

## Extension commands (nextest & co.)

Like cargo itself, any unrecognized subcommand is treated as a cargo
extension: sources are synced, then `cargo <subcommand> <args...>` runs on
the remote, where cargo resolves the `cargo-<subcommand>` binary.

```console
$ cargo remote nextest run --profile fast
$ cargo remote llvm-cov --workspace
```

The extension must be installed on the remote (e.g. `ssh my-server
'$HOME/.cargo/bin/cargo install cargo-nextest --locked'`); if it is
missing, cargo's own "no such command" error is what you'll see. Note that
for extension commands, cargo-remote's own options must come *before* the
subcommand — everything after goes to the extension verbatim. Extension
commands never copy artifacts back (like `test`, the binaries run on the
remote).

## Dry runs

`-n/--dry-run` prints exactly what would happen — the local sync pipeline,
the remote command, the copy-back probe — without touching the network.
Remote requirement checks (`arch`, `min_rust_version`) are skipped since
they need a connection.

## Output and exit codes

All cargo-remote status/diagnostics go to **stderr**; stdout carries only
the remote command's own output (and the `hosts` listing), so stdout stays
pipeable. Exit codes: the remote command's exit status for build-type
commands, 0/1 for `probe`, 2 for local errors (bad config, no host, sync
failure).
