//! The placeholders of a snippet: `{{name}}`, or `{{name:default}}`, filled in when it is sent.

/// A value a snippet asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variable<'a> {
    pub name: &'a str,
    pub default: Option<&'a str>,
}

/// The placeholder starting at the head of `text`, which begins with `{{`, and the bytes it takes.
fn placeholder(text: &str) -> Option<(Variable<'_>, usize)> {
    let end = text.find("}}")?;
    let inner = &text[2..end];
    let (name, default) = match inner.split_once(':') {
        Some((name, default)) => (name, Some(default)),
        None => (inner, None),
    };
    let name = name.trim();
    // Anything else between braces is text that happens to have braces in it.
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '));
    valid.then_some((Variable { name, default }, end + 2))
}

/// Go through `body`, handing each stretch of plain text and each placeholder to `visit`.
fn walk<'a>(body: &'a str, mut visit: impl FnMut(Result<&'a str, Variable<'a>>)) {
    let mut rest = body;
    while let Some(at) = rest.find("{{") {
        let (before, from) = rest.split_at(at);
        match placeholder(from) {
            Some((variable, length)) => {
                visit(Ok(before));
                visit(Err(variable));
                rest = &from[length..];
            },
            None => {
                let kept = at + 2;
                visit(Ok(&rest[..kept]));
                rest = &rest[kept..];
            },
        }
    }
    visit(Ok(rest));
}

/// What `body` asks for, each name once, in the order first mentioned.
pub fn variables(body: &str) -> Vec<Variable<'_>> {
    let mut found: Vec<Variable<'_>> = Vec::new();
    walk(body, |piece| {
        if let Err(variable) = piece
            && !found.iter().any(|known| known.name == variable.name)
        {
            found.push(variable);
        }
    });
    found
}

/// `body` with each placeholder replaced by the value given for its name, or its default.
pub fn expand(body: &str, values: &[(&str, &str)]) -> String {
    let mut text = String::with_capacity(body.len());
    walk(body, |piece| match piece {
        Ok(plain) => text.push_str(plain),
        Err(variable) => {
            let value =
                values.iter().find(|(name, _)| *name == variable.name).map(|(_, value)| *value);
            text.push_str(value.or(variable.default).unwrap_or_default());
        },
    });
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var<'a>(name: &'a str, default: Option<&'a str>) -> Variable<'a> {
        Variable { name, default }
    }

    #[test]
    fn plain_text_has_no_variables() {
        assert!(variables("uptime; ls -la").is_empty());
        assert_eq!(expand("uptime; ls -la", &[]), "uptime; ls -la");
    }

    #[test]
    fn variables_come_once_in_order_with_their_defaults() {
        let body = "scp {{file}} {{ host : }}:{{dir:/tmp}}; rm {{file}}";
        assert_eq!(
            variables(body),
            [var("file", None), var("host", Some(" ")), var("dir", Some("/tmp"))]
        );
    }

    #[test]
    fn values_replace_placeholders_and_defaults_fill_the_rest() {
        let body = "scp {{file}} {{host}}:{{dir:/tmp}}; rm {{file}}";
        assert_eq!(
            expand(body, &[("file", "a.txt"), ("host", "box")]),
            "scp a.txt box:/tmp; rm a.txt",
        );
    }

    #[test]
    fn a_blank_value_given_is_kept_over_the_default() {
        assert_eq!(expand("x{{a:b}}", &[("a", "")]), "x");
    }

    #[test]
    fn braces_that_are_no_placeholders_stay_as_they_are() {
        for text in ["echo {{", "{{}}", "awk '{{ print $1 }}'x", "{{a&b}} y", "a}}b{{"] {
            assert_eq!(expand(text, &[]), text, "{text}");
        }
    }

    #[test]
    fn multibyte_text_is_cut_only_between_characters() {
        assert_eq!(expand("你好 {{名字:世界}}！", &[]), "你好 世界！");
        assert_eq!(variables("{{名字}}"), [var("名字", None)]);
    }
}
