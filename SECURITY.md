# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities by opening a **GitHub Security
Advisory** on this repository (Security tab → "Report a vulnerability"), not
a public issue. You can expect an acknowledgement within a week.

## Trust model (read before deploying)

cargo-remote is a remote-build tool: the server you point it at runs your
builds, so it is *trusted* to execute project code (build scripts,
proc-macros) — that's the point of the tool. The hardening below is what it
does on top of that:

- **Host-key verification** against `~/.ssh/known_hosts` (and the
  system-wide file), TOFU prompt for unknown keys, hard failure on changed
  keys.
- **Config trust split**: files inside a cloned repository
  (`.cargo-remote.toml`) may only set build-shaping keys; `host`,
  `remote_dir`, `cargo_path` and `copy_back` come from your global config or
  CLI unless you opt in with `trust_project_config = true`.
- Values crossing into remote shell command lines are single-quoted or
  charset-validated (see `shell_quote`, `quote_remote_path`,
  `validate_env_entries`, `is_valid_toolchain`, `is_valid_cargo_path` in
  `src/runner.rs`).
- The remote sync agent is verified by a hash of the binary, its directory
  kept at 0700, all wire paths confined to the sync root, and small files
  are content-verified on otherwise-no-op syncs.
- Copied-back binaries land with unguessable temp names, masked permissions,
  and a size cap.

## Scope notes / known limitations

- SSH agent *forwarding* is intentionally absent.
- On legacy SSH servers without rsa-sha2 (pre-2018), RSA keys are refused
  instead of downgrading to SHA-1 signatures.
- The `rsa` crate in the dependency tree carries RUSTSEC-2023-0071 (Marvin
  timing sidechannel); its risk profile does not match client-side SSH
  signing, but we track upstream and will drop the `rsa` feature if russh
  no longer needs it.
