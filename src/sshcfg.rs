//! Listing host names from the user's ssh config.

/// Host names declared in ~/.ssh/config, in file order, deduplicated.
/// Wildcard (`Host *`), negated (`!x`) and pattern entries are skipped.
pub fn config_hosts() -> anyhow::Result<Vec<String>> {
    let home =
        std::env::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    let path = home.join(".ssh").join("config");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(parse_hosts(&text))
}

pub(crate) fn parse_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(keyword) = words.next() else { continue };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for w in words {
            if w.contains('*') || w.contains('?') || w.starts_with('!') {
                continue;
            }
            let h = w.to_string();
            if !hosts.contains(&h) {
                hosts.push(h);
            }
        }
    }
    hosts
}

/// The effective `StrictHostKeyChecking` value for `alias` from
/// ~/.ssh/config, with OpenSSH semantics (scanned top-down, first `Host`
/// block whose globs match the alias wins; `*`/`?` globs, `!` negation).
///
/// russh-config collapses `ask`/`accept-new` to `true`; treating those as
/// strict permanently locks new hosts out, so this one key is read with our
/// own scan and `connect` maps ask/accept-new to the interactive TOFU flow.
pub(crate) fn strict_host_key_setting(alias: &str) -> Option<String> {
    let home = std::env::home_dir()?;
    let text = std::fs::read_to_string(home.join(".ssh/config")).ok()?;
    strict_host_key_setting_in(&text, alias)
}

fn strict_host_key_setting_in(text: &str, alias: &str) -> Option<String> {
    let mut active = false; // inside a Host block matching `alias`
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(kw) = words.next() else { continue };
        if kw.eq_ignore_ascii_case("host") {
            active = false;
            for pat in words {
                if let Some(neg) = pat.strip_prefix('!') {
                    if glob_match(neg, alias) {
                        active = false;
                        break;
                    }
                } else if glob_match(pat, alias) {
                    active = true;
                }
            }
            continue;
        }
        if active && kw.eq_ignore_ascii_case("stricthostkeychecking") {
            return words.next().map(|w| w.to_ascii_lowercase());
        }
    }
    None
}

/// `*`/`?` glob, full-string match.
fn glob_match(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut star_pos) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            star_pos = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_pos += 1;
            si = star_pos;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts() {
        let cfg = r#"
# comment
Host dev-box
    HostName 192.168.1.10
    User me

Host dev-box prod-box *.internal !secret
  Host another
    # indented comment
Host *
    ServerAliveInterval 30
"#;
        assert_eq!(
            parse_hosts(cfg),
            vec!["dev-box", "prod-box", "another"]
        );
    }

    #[test]
    fn empty_config() {
        assert!(parse_hosts("").is_empty());
    }

    #[test]
    fn glob_matcher() {
        assert!(glob_match("dev", "dev"));
        assert!(glob_match("dev*", "dev-box"));
        assert!(glob_match("*-box", "dev-box"));
        assert!(glob_match("d?v", "dev"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("dev", "devx"));
        assert!(!glob_match("d?x", "dev"));
        assert!(!glob_match("prod", "dev"));
        assert!(glob_match("*x", "x"));
        assert!(!glob_match("*y", "x"));
    }

    #[test]
    fn strict_setting_scans_like_openssh() {
        let cfg = r#"
Host *
    ServerAliveInterval 30

Host dev-box
    StrictHostKeyChecking ask

Host strict-*
    StrictHostKeyChecking yes

Host wild-*
    StrictHostKeyChecking accept-new
"#;
        assert_eq!(strict_host_key_setting_in(cfg, "dev-box").as_deref(), Some("ask"));
        assert_eq!(strict_host_key_setting_in(cfg, "strict-9").as_deref(), Some("yes"));
        assert_eq!(strict_host_key_setting_in(cfg, "wild-x").as_deref(), Some("accept-new"));
        assert_eq!(strict_host_key_setting_in(cfg, "other"), None);
        // Negation wins over globs.
        let cfg2 = "Host * !prod\n StrictHostKeyChecking ask\n";
        assert_eq!(strict_host_key_setting_in(cfg2, "prod"), None);
        assert_eq!(strict_host_key_setting_in(cfg2, "dev").as_deref(), Some("ask"));
    }
}
