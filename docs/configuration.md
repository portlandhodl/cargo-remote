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

## Quick examples

One project, one build server (committed alongside your code, like
`.config/nextest.toml`):

```toml
# .config/cargo-remote.toml
host = "anchorwatch-dev"
```

All projects default to your build box:

```toml
# ~/.config/cargo-remote/config.toml
host = "anchorwatch-dev"
```

With that, `cargo remote build --release` is the whole command line — the
host, and anything else you configured, comes along automatically.

Pin the remote so compiled outputs always match your machine:

```toml
# .config/cargo-remote.toml
host = "anchorwatch-dev"
arch = "host"              # remote must have the same arch as this machine
min_rust_version = "1.85"  # remote toolchain must be at least this version
```

A CI-friendly variant:

```toml
# .config/cargo-remote.toml
host = "ci-builder"
sync_status = false  # no live progress line in CI logs
verbose = true       # echo each step instead
```

## Reference

| Key | Type | Default | CLI equivalent | Meaning |
| --- | --- | --- | --- | --- |
| `host` | string | — (required somewhere) | `-r/--remote` | Build server: an alias from `~/.ssh/config` or a plain host name |
| `remote_dir` | string | `"~/remote-builds"` | `--remote-dir` | Base directory on the remote for per-project build dirs |
| `cargo_path` | string | `$CARGO_REMOTE_CARGO` or `"$HOME/.cargo/bin/cargo"` | — | Path to cargo on the remote |
| `transfer` | string | `"auto"` | `--transfer` | Source sync mode: `auto`, `rsync`, `rsync-z`, `tar`, `tar-gz`, `tar-zstd`, `tar-inc` |
| `copy_back` | bool | `true` | `--no-copy-back` | After `build`, copy produced binaries into local `target/<profile>/` |
| `sync` | bool | `true` | `--no-sync` | Sync sources before running the remote command |
| `exclude_git` | bool | `false` | `--exclude-git` | Also exclude `.git/` from the sync (`target/` is always excluded) |
| `exclude` | list of strings | `[]` | `--exclude` | Extra root-anchored paths to exclude (accumulates across files) |
| `env` | list of strings | `[]` | `--env` | `KEY=VALUE` pairs set for the remote build (accumulates; `RUSTFLAGS` is always forwarded when set locally) |
| `toolchain` | string | — | `--toolchain` | Rust toolchain on the remote via the rustup shim, e.g. `"nightly"` |
| `verbose` | bool | `false` | `-v/--verbose` | Logs: echo each step/command as it runs |
| `sync_status` | bool | `true` | `--no-sync-status` | Live sync progress line (bytes, speed, ETA) on stderr; only drawn on terminals |
| `arch` | string | — | — | Required remote machine architecture (`uname -m`): e.g. `"x86_64"`, `"aarch64"`, or `"host"` for this machine's arch |
| `min_rust_version` | string | — | — | Minimum remote toolchain version, e.g. `"1.85"`; checked against the remote `cargo --version` |

## The `host` value

`host` names a server the built-in SSH client can reach. Two forms:

- **An alias from `~/.ssh/config`** — run `cargo remote hosts` to list them.
  HostName, User, Port, IdentityFile, ProxyCommand, and the key-verification
  settings are all honored.
- **A manual entry** — a plain host name (e.g. `builder.example.com`), using
  your local username and the default port/identity files.

Resolution order for the host: `-r` flag → `$CARGO_REMOTE_HOST` →
`.cargo-remote.toml` → `.config/cargo-remote.toml` → global config.

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

- `copy_back` from the config can be overridden per run with
  `--copy-back` / `--no-copy-back`; `sync` likewise with `--no-sync`.
  Boolean CLI flags can only *turn on* behavior (`--verbose`,
  `--exclude-git`, `--no-sync-status`), so a config `true` for those stays
  on for that run.
- The agent (the helper binary cargo-remote installs on the remote on first
  use) is unaffected by these files; it always lives at
  `<remote_dir>/.cargo-remote/agent`.
