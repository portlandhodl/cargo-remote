# cargo-remote documentation

cargo-remote compiles Rust projects on a remote server over SSH, as if it
were local: sources sync incrementally, the cargo command runs on the
server, and built binaries are copied back into the local `target/`
directory.

## Contents

| Document | What's in it |
| --- | --- |
| [commands.md](commands.md) | Every subcommand: `build`, `check`/`clippy`/`test`/`run`/`bench`/`doc`, `clean`, `sync`, `hosts`, `probe` (host support check), cargo extension passthrough (e.g. nextest), dry runs, output/exit-code conventions |
| [configuration.md](configuration.md) | Config files (`.config/cargo-remote.toml`, `.cargo-remote.toml`, `~/.config/cargo-remote/config.toml`), precedence, every key, host resolution, `arch` / `min_rust_version` enforcement |

The repository [README](../README.md) covers installation and the big
picture (transfer modes, how the sync works, SSH compatibility).

## Quick orientation for agents

Typical flows, assuming a configured host (see configuration.md):

```console
$ cargo remote probe                 # is the host usable? exit 0 = ready
$ cargo remote build --release       # sync, build remotely, copy binaries back
$ cargo remote test                  # sync, run the test suite on the server
$ cargo remote nextest run           # cargo extensions work the same way
$ cargo remote -n build              # dry-run: print what would happen, no network
```

Machine-readable behavior to rely on:

- All cargo-remote diagnostics and status go to **stderr**; **stdout** is
  only the remote command's output (plus the `hosts` listing).
- Exit codes: remote command's exit status (build-type commands), 0/1 for
  `probe` (1 = at least one hard check failed), 2 for local errors (no
  host, bad config, sync failure).
- `-n/--dry-run` never touches the network and prints each step with a
  `(dry-run) +` prefix — use it to inspect the effective config and the
  exact remote command line.
- Live sync progress draws on stderr only when it's a terminal; piped
  output stays clean. `--no-sync-status` forces it off.
