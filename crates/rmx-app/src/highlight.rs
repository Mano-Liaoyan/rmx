//! Coloring source text, off the main thread.
//!
//! What comes out says what each run of text is, not its color, so that changing theme needs no
//! second pass.

use std::sync::OnceLock;

use rmx_ui::theme::Token;
use rmx_ui::widgets::{Highlight, HighlightLine, TokenRun};
use syntect::parsing::{
    ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxDefinition, SyntaxReference, SyntaxSet,
};

/// The longest text that is colored: it takes a while, and nobody reads that much of it.
const TEXT_LIMIT: usize = 2 * 1024 * 1024;
/// A longer line is left plain: minified files spend seconds in their first few lines.
const LINE_LIMIT: usize = 4096;

/// For the many files that are lines of `key = value`, which the bundled syntaxes do not cover.
const CONFIG_SYNTAX: &str = r#"%YAML 1.2
---
name: Config
file_extensions: [toml, ini, conf, cfg, cnf, env, service, socket, timer, properties, desktop]
scope: source.config
contexts:
  main:
    - match: '^\s*[#;].*$'
      scope: comment.line.config
    - match: '^\s*\[.*\]'
      scope: entity.name.type.section.config
    - match: '^\s*[\w.\-"]+(?=\s*[=:])'
      scope: keyword.other.key.config
    - match: '"(\\.|[^"\\])*"|''[^'']*'''
      scope: string.quoted.config
    - match: '\b(true|false|yes|no|on|off|null)\b'
      scope: constant.language.config
    - match: '\b\d+(\.\d+)?\b'
      scope: constant.numeric.config
"#;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        match SyntaxDefinition::load_from_str(CONFIG_SYNTAX, true, None) {
            Ok(config) => builder.add(config),
            Err(err) => log::warn!("The config syntax does not load: {err}"),
        }
        builder.build()
    })
}

/// Which scopes count as which token, the more specific first.
fn classes() -> &'static [(Scope, Token)] {
    static CLASSES: OnceLock<Vec<(Scope, Token)>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        [
            ("comment", Token::Comment),
            ("string", Token::String),
            ("constant.numeric", Token::Number),
            ("constant", Token::Constant),
            ("entity.name.function", Token::Function),
            ("support.function", Token::Function),
            ("variable.function", Token::Function),
            ("entity.name.tag", Token::Keyword),
            ("entity.other.attribute-name", Token::Constant),
            ("entity.name", Token::Type),
            ("entity.other.inherited-class", Token::Type),
            ("support.type", Token::Type),
            ("support.class", Token::Type),
            ("storage.type", Token::Keyword),
            ("storage", Token::Keyword),
            ("keyword.operator", Token::Keyword),
            ("keyword", Token::Keyword),
        ]
        .into_iter()
        .filter_map(|(name, token)| Some((Scope::new(name).ok()?, token)))
        .collect()
    })
}

/// What the innermost scope that means anything says the text is.
fn token_of(stack: &ScopeStack) -> Option<Token> {
    let classes = classes();
    stack.as_slice().iter().rev().find_map(|scope| {
        classes.iter().find(|(prefix, _)| prefix.is_prefix_of(*scope)).map(|(_, token)| *token)
    })
}

/// What other names a file's syntax goes by, for those the bundled syntaxes do not list.
fn alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "ts" | "tsx" | "mts" | "cts" | "mjs" | "cjs" | "jsx" => "js",
        "jsonc" | "json5" => "json",
        "zsh" | "zshrc" | "bashrc" | "profile" | "bash_profile" | "zprofile" => "sh",
        "dockerfile" | "containerfile" | "gitconfig" | "editorconfig" | "npmrc" | "env" => "conf",
        _ => return None,
    })
}

/// The syntax of a file called `name` that starts with `first_line`.
fn syntax_for<'a>(set: &'a SyntaxSet, name: &str, first_line: &str) -> Option<&'a SyntaxReference> {
    let base = name.rsplit('/').next().unwrap_or(name).trim_start_matches('.').to_lowercase();
    let extension = base.rsplit('.').next().unwrap_or(&base);
    let found = [extension, base.as_str()]
        .into_iter()
        .find_map(|candidate| set.find_syntax_by_extension(alias(candidate).unwrap_or(candidate)));
    found
        .or_else(|| set.find_syntax_by_first_line(first_line))
        .filter(|syntax| syntax.name != "Plain Text")
}

/// Add the run from `start` to `end` of a line to `out`, which keeps runs of one token together.
fn push_run(out: &mut Vec<TokenRun>, from: usize, start: usize, end: usize, token: Option<Token>) {
    let Some(token) = token.filter(|_| start < end) else { return };
    let (start, end) = (start as u32, end as u32);
    match out[from..].last_mut() {
        Some(last) if last.token == token && last.end == start => last.end = end,
        _ => out.push(TokenRun { start, end, token }),
    }
}

/// Which token each part of `text`, a file called `name`, is. `None` for what is too long, or in a
/// language nothing is known about.
pub fn highlight(name: &str, text: &str) -> Option<Highlight> {
    if text.len() > TEXT_LIMIT {
        return None;
    }
    let set = syntaxes();
    let syntax = syntax_for(set, name, text.lines().next().unwrap_or(""))?;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut result = Highlight::default();
    for line in text.split_inclusive('\n') {
        let len = line.trim_end_matches('\n').trim_end_matches('\r').len();
        let from = result.runs.len();
        if line.len() <= LINE_LIMIT {
            let ops: Vec<(usize, ScopeStackOp)> = state.parse_line(line, set).ok()?;
            let mut at = 0;
            for (position, op) in ops {
                let position = position.min(len);
                push_run(&mut result.runs, from, at, position, token_of(&stack));
                at = at.max(position);
                stack.apply(&op).ok()?;
            }
            push_run(&mut result.runs, from, at, len, token_of(&stack));
        }
        result.lines.push(HighlightLine { first_run: from as u32, len: len as u32 });
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(name: &str, text: &str) -> Vec<(String, Token)> {
        let found = highlight(name, text).expect("a language it knows");
        let line = found.line(0).expect("a line");
        line.1
            .iter()
            .map(|run| (text[run.start as usize..run.end as usize].to_owned(), run.token))
            .collect()
    }

    #[test]
    fn code_is_told_apart_by_what_it_is() {
        let found = tokens("a.py", "def f(): return 'x' # note\n");
        assert!(found.contains(&("def".to_owned(), Token::Keyword)), "{found:?}");
        assert!(
            found.iter().any(|(text, token)| text.contains('x') && *token == Token::String),
            "{found:?}"
        );
        assert!(
            found.iter().any(|(text, token)| text.contains("note") && *token == Token::Comment),
            "{found:?}"
        );
    }

    #[test]
    fn config_files_have_their_keys_and_comments_marked() {
        let found = tokens("Cargo.toml", "name = \"rmx\"\n");
        assert!(
            found.iter().any(|(text, token)| text == "name" && *token == Token::Keyword),
            "{found:?}"
        );
        assert!(found.iter().any(|(_, token)| *token == Token::String), "{found:?}");
        assert_eq!(tokens(".env", "# secret\n")[0].1, Token::Comment);
    }

    #[test]
    fn the_language_comes_from_the_name_or_the_first_line() {
        assert!(highlight("run", "#!/bin/sh\necho hi\n").is_some());
        assert!(highlight("x.ts", "const a = 1;\n").is_some());
        assert!(highlight("notes.txt", "just words\n").is_none());
    }

    #[test]
    fn every_line_is_accounted_for_without_its_ending() {
        let found = highlight("a.rs", "fn a() {}\r\n\nlet b;").unwrap();
        assert_eq!(found.lines.iter().map(|line| line.len).collect::<Vec<_>>(), [9, 0, 6]);
    }

    #[test]
    fn a_huge_line_stays_plain_and_the_rest_is_still_colored() {
        let text = format!("{}\nfn a() {{}}\n", "x".repeat(LINE_LIMIT * 2));
        let found = highlight("a.rs", &text).unwrap();
        assert!(found.line(0).unwrap().1.is_empty());
        assert!(!found.line(1).unwrap().1.is_empty());
    }
}
