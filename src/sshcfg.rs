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
}
