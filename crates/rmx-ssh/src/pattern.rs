//! The `*` and `?` glob matching shared by `known_hosts` and `ssh_config`.

/// Whether `text` matches `pattern`, where `*` matches any run of characters
/// and `?` matches exactly one.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Where to resume after the last `*`: its pattern index and the text index
    // it has consumed up to.
    let mut backtrack = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                backtrack = Some((p, t));
                p += 1;
            },
            Some(&c) if c == '?' || c == text[t] => {
                p += 1;
                t += 1;
            },
            _ => match backtrack {
                Some((star, consumed)) => {
                    p = star + 1;
                    t = consumed + 1;
                    backtrack = Some((star, consumed + 1));
                },
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Whether `candidate` matches a list of patterns where a leading `!` negates:
/// at least one positive pattern must match and no negated pattern may.
pub(crate) fn pattern_list_matches<'a>(
    patterns: impl IntoIterator<Item = &'a str>,
    candidate: &str,
) -> bool {
    let mut matched = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_match(negated, candidate) {
                return false;
            }
        } else if glob_match(pattern, candidate) {
            matched = true;
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*.example.com", "a.b.example.com"));
        assert!(!glob_match("*.example.com", "example.com"));
        assert!(glob_match("web?", "web1"));
        assert!(!glob_match("web?", "web"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn negation_needs_a_positive_match() {
        assert!(pattern_list_matches(["*", "!bad"], "good"));
        assert!(!pattern_list_matches(["*", "!bad"], "bad"));
        assert!(!pattern_list_matches(["!bad"], "good"));
    }
}
