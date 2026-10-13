# Configuration

cargo-remote works out of the box with a single `-r <HOST>` flag, but the
goal is for it to be as easy as cargo: type `cargo remote build` and be
done. Config files let you set everything once — per project or globally —
so no flags are needed at all.

## File locations

Settings are read from these files, lowest to highest precedence:

| Precedence | File | Scope |
| --- | --- | --- |
| 1 (lowest) | `$XDG_CONFIG_HOME/cargo-remote/config.toml` (usually `~/.config/cargo-remote/config.toml`) | All your projects |
| 2 | `<workspace>/.config/cargo-remote.toml` | This project (nextest-style location) |
| 3 | `<workspace>/.cargo-remote.toml` | This project (workspace root) |

Above all files: the `$CARGO_REMOTE_HOST` environment variable (host only),
then CLI flags (highest precedence).

Scalar keys from a higher-precedence file override lower ones. List keys
(`exclude`, `env`) accumulate across files. Unknown keys are rejected, so a
typo is an error instead of silently ignored.

## Security model: which file may set which keys

Project-level files usually live in a repository you cloned — i.e. they are
**untrusted input**. cargo-remote therefore splits keys in two groups:

- **Remote-control keys** — `host`, `remote_dir`, `cargo_path`,
  `copy_back`, `trust_project_config` — are honored only from the *global*
  config, the CLI, or the environment. A project file that sets them is
  reported with a warning and the keys are ignored.
- **Build-shaping keys** — `toolchain`, `env`, `transfer`, `sync`,
  `exclude_git`, `exclude`, `arch`, `min_rust_version`, `verbose`,
  `sync_status` — may come from project files: they pick how *your* code is
  built on *your* server, something the repo's own `build.rs` could already
  do during any remote build.

If your team intentionally commits `host = "…"` in a project config (the
`direnv`-style workflow), opt in once, globally:

```toml
# ~/.config/cargo-remote/config.toml
trust_project_config = true
```

With trust enabled, a project-sourced `host` still drops `copy_back` to
**off** by default (binaries produced by a project-chosen server are not
copied into your local `target/` unless you pass `--copy-back` or set
`copy_back` in the global config).

## Quick examples

All projects default to your build box:

```toml
# ~/.config/cargo-remote/config.toml
host = "anchorwatch-dev"
```

Per-project pinning (committed alongside your code, like
`.config/nextest.toml`):

```toml
# .config/cargo-remote.toml
toolchain = "nightly"
exclude = ["data/big-fixture"]
```

With the global `host`, `cargo remote build --release` is the whole command
line — the host, and anything else you configured, comes along
automatically.

Pin the remote so compiled outputs always match your machine:

```toml
# ~/.config/cargo-remote/config.toml
host = "anchorwatch-dev"
arch = "host"              # remote must have the same arch as this machine
min_rust_version = "1.85"  # remote toolchain must be at least this version
```

A CI-friendly variant:

```toml
# ~/.config/cargo-remote/config.toml
host = "ci-builder"
sync_status = false  # no live progress line in CI logs
verbose = true       # echo each step instead (also reveals env values)
```

## Reference

| Key | Type | Default | CLI equivalent | Project files? | Meaning |
| --- | --- | --- | --- | --- | --- |
| `host` | string | — (required somewhere) | `-r/--remote` | trusted only | Build server: an alias from `~/.ssh/config` or a plain host name |
| `remote_dir` | string | `"~/remote-builds"` | `--remote-dir` | trusted only | Base directory on the remote for per-project build dirs |
| `cargo_path` | string | `$CARGO_REMOTE_CARGO` or `"$HOME/.cargo/bin/cargo"` | — | trusted only | Path to cargo on the remote (charset-validated) |
| `copy_back` | bool | `true` | `--no-copy-back` | trusted only | After `build`, copy produced binaries into local `target/<profile>/` |
| `transfer` | string | `"auto"` | `--transfer` | yes | Source sync mode: `auto`, `rsync`, `rsync-z`, `tar`, `tar-gz`, `tar-zstd`, `tar-inc` |
| `sync` | bool | `true` | `--no-sync` | yes | Sync sources before running the remote command |
| `exclude_git` | bool | `false` | `--exclude-git` | yes | Also exclude `.git/` from the sync (`target/` is always excluded) |
| `exclude` | list of strings | `[]` | `--exclude` | yes | Extra root-anchored paths to exclude (accumulates across files) |
| `env` | list of strings | `[]` | `--env` | yes | `KEY=VALUE` pairs set for the remote build (accumulates; `RUSTFLAGS` is always forwarded when set locally) |
| `toolchain` | string | — | `--toolchain` | yes | Rust toolchain on the remote via the rustup shim, e.g. `"nightly"` |
| `verbose` | bool | `false` | `-v/--verbose` | yes | Logs: echo each step/command; also reveals `env` values in status lines |
| `sync_status` | bool | `true` | `--no-sync-status` | yes | Live sync progress line (bytes, speed, ETA) on stderr; only drawn on terminals |
| `arch` | string | — | — | yes | Required remote machine architecture (`uname -m`): e.g. `"x86_64"`, `"aarch64"`, or `"host"` |
| `min_rust_version` | string | — | — | yes | Minimum remote toolchain version, e.g. `"1.85"`; checked against the remote `cargo --version` |
| `trust_project_config` | bool | `false` | — | never | Opt-in (global config only): honor the remote-control keys from project files |

Values that end up inside remote shell command lines are validated
(`toolchain`, `cargo_path`, `env` keys) and/or single-quoted
(`remote_dir`, paths, `env` values) before use.

## The `host` value

`host` names a server the built-in SSH client can reach. Two forms:

- **An alias from `~/.ssh/config`** — run `cargo remote hosts` to list them.
  HostName, User, Port, IdentityFile, ProxyCommand, and the key-verification
  settings are all honored.
- **A manual entry** — a plain host name (e.g. `builder.example.com`), using
  your local username and the default port/identity files.

Resolution order for the host: `-r` flag → `$CARGO_REMOTE_HOST` → global
config. Project-level `host` is honored only with
`trust_project_config = true` (see the security model above).

## Matching compiled outputs: `arch` and `min_rust_version`

`build` copies binaries back into your local `target/` directory, which is
only useful when the remote produces binaries your machine can run. Two
constraints keep that from going wrong silently:

- **`arch`** is checked against the remote's `uname -m` right after
  connecting, before anything is synced or built. `"host"` resolves to the
  local machine's architecture. A mismatch is a hard error:

  ```text
  cargo-remote: error: remote arch 'aarch64' does not match required arch 'x86_64' (config `arch`); compiled outputs would not match
  ```

  When `arch` is *not* set and you run `build` with copy-back enabled, a
  differing remote arch produces a warning instead of an error (cross-arch
  setups can silence it by setting `arch` to the remote's value).

- **`min_rust_version`** is compared against the remote `cargo --version`
  (respecting `toolchain`). Too old is a hard error before any work starts:

  ```text
  cargo-remote: error: remote toolchain is cargo 1.70.0 (...), but the config requires min_rust_version = "1.85"
  ```

Both checks need the network, so they are skipped in `--dry-run` mode. Run
`cargo remote probe` to verify them — along with connection, tools, agent
and disk — against the live host (see
[commands.md](commands.md#probe)).

## Notes

- Runs are serialized per project+host with a lock in
  `~/.cache/cargo-remote/locks` (a stale lock from a crashed process is
  reclaimed automatically on unix).
- The sync agent is verified by a hash of the binary (not just a version
  string) on every run; a mismatch re-uploads it. The agent directory on the
  remote is kept at mode 0700.
- Delta sync double-checks small files by content signature, so a file
  modified on the server with preserved size/mtime is still resynced.
  Downloaded artifacts are written with unguessable temp names and
  masked/validated permissions.
- Status lines show `env` values as `KEY=***` (they are usually secrets);
  `-v/--verbose` reveals them.
- After a build, if the remote `Cargo.lock` differs from local, the remote
  copy is saved to `Cargo.lock.remote` in the project root for review;
  `--update-lockfile` applies it to `Cargo.lock` directly (atomic write).
- `copy_back` from the config can be overridden per run with
  `--copy-back` / `--no-copy-back`; `sync` likewise with `--no-sync`.
  Boolean CLI flags can only *turn on* behavior (`--verbose`,
  `--exclude-git`, `--no-sync-status`), so a config `true` for those stays
  on for that run.
- The agent (the helper binary cargo-remote installs on the remote on first
  use) is unaffected by these files; it always lives at
  `<remote_dir>/.cargo-remote/agent`.
