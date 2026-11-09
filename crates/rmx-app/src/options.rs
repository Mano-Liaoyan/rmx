//! Formats a connection's settings are stored and typed in: the options column and port forwards.

use std::collections::BTreeMap;

use rmx_vault::Tunnel;

/// How the form shows a key that is already in the vault.
pub const STORED_KEY: &str = "vault:";

/// A flat JSON object of strings, as the `options` column holds.
pub fn encode(map: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        quote(key, &mut out);
        out.push(':');
        quote(value, &mut out);
    }
    out.push('}');
    out
}

fn quote(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Read what [`encode`] wrote. Anything else gives what could be understood of it.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut chars = text.chars().peekable();
    let mut strings = Vec::new();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut string = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => string.push('\n'),
                    Some('t') => string.push('\t'),
                    Some('r') => string.push('\r'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        let decoded = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                        string.extend(decoded);
                    },
                    Some(other) => string.push(other),
                    None => break,
                },
                c => string.push(c),
            }
        }
        strings.push(string);
    }
    let mut strings = strings.into_iter();
    while let (Some(key), Some(value)) = (strings.next(), strings.next()) {
        map.insert(key, value);
    }
    map
}

fn parse_port(text: &str, spec: &str) -> Result<u16, String> {
    text.parse().map_err(|_| format!("{spec}: {text} is not a port"))
}

/// `L8080:db:5432`, `L127.0.0.1:8080:db:5432`, `R9000:localhost:80`, `D1080`, comma separated.
pub fn parse_forwards(text: &str) -> Result<Vec<Tunnel>, String> {
    let mut tunnels = Vec::new();
    for spec in text.split(',').map(str::trim).filter(|spec| !spec.is_empty()) {
        let mut chars = spec.chars();
        let letter = chars.next().map(|c| c.to_ascii_uppercase());
        let parts: Vec<&str> = chars.as_str().split(':').collect();
        let (kind, bind, target) = match (letter, parts.as_slice()) {
            (Some('D'), [port]) => ("dynamic", (None, *port), None),
            (Some('D'), [host, port]) => ("dynamic", (Some(*host), *port), None),
            (Some(letter @ ('L' | 'R')), [port, to, to_port]) => {
                (if letter == 'L' { "local" } else { "remote" }, (None, *port), Some((*to, *to_port)))
            },
            (Some(letter @ ('L' | 'R')), [host, port, to, to_port]) => (
                if letter == 'L' { "local" } else { "remote" },
                (Some(*host), *port),
                Some((*to, *to_port)),
            ),
            _ => return Err(format!("{spec}: write it like L8080:db:5432, D1080 or R9000:localhost:80")),
        };
        let (target_host, target_port) = match target {
            Some((host, port)) => (Some(host.to_owned()), Some(parse_port(port, spec)?)),
            None => (None, None),
        };
        tunnels.push(Tunnel {
            name: spec.to_owned(),
            kind: kind.to_owned(),
            bind_host: bind.0.unwrap_or("127.0.0.1").to_owned(),
            bind_port: parse_port(bind.1, spec)?,
            target_host,
            target_port,
            autostart: true,
            ..Default::default()
        });
    }
    Ok(tunnels)
}

/// A tunnel as the form writes it.
pub fn forward_text(tunnel: &Tunnel) -> String {
    let bind = format!("{}:{}", tunnel.bind_host, tunnel.bind_port);
    match (tunnel.kind.as_str(), &tunnel.target_host, tunnel.target_port) {
        ("local", Some(host), Some(port)) => format!("L{bind}:{host}:{port}"),
        ("remote", Some(host), Some(port)) => format!("R{bind}:{host}:{port}"),
        _ => format!("D{bind}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_survive_awkward_text() {
        let mut map = BTreeMap::new();
        map.insert("identity_files".to_owned(), "/a \"b\"\n/c\\d".to_owned());
        map.insert("auth".to_owned(), "agent".to_owned());
        assert_eq!(parse(&encode(&map)), map);
    }

    #[test]
    fn forwards_parse_with_and_without_a_bind_address() {
        let tunnels = parse_forwards("L8080:db:5432, r0.0.0.0:9000:localhost:80 , D1080").unwrap();
        assert_eq!(tunnels.len(), 3);
        assert_eq!(tunnels[0].kind, "local");
        assert_eq!(tunnels[0].bind_host, "127.0.0.1");
        assert_eq!(tunnels[0].target_port, Some(5432));
        assert_eq!(tunnels[1].kind, "remote");
        assert_eq!(tunnels[1].bind_host, "0.0.0.0");
        assert_eq!(tunnels[2].kind, "dynamic");
        assert_eq!(tunnels[2].target_host, None);
        assert_eq!(forward_text(&tunnels[0]), "L127.0.0.1:8080:db:5432");
    }

    #[test]
    fn forwards_written_back_parse_the_same() {
        for tunnel in parse_forwards("L8080:db:5432, R9000:localhost:80, D1080").unwrap() {
            let again = parse_forwards(&forward_text(&tunnel)).unwrap();
            assert_eq!(forward_text(&again[0]), forward_text(&tunnel));
        }
    }

    #[test]
    fn a_bad_forward_is_reported() {
        assert!(parse_forwards("X1:2:3").is_err());
        assert!(parse_forwards("L8080:db").is_err());
        assert!(parse_forwards("Labc:db:80").is_err());
    }
}
