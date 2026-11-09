//! Lining two texts up, line by line, for showing side by side.

use std::time::{Duration, Instant};

use rmx_ui::widgets::{DiffLine, DiffRow};
use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

/// How long to look for the shortest set of changes. Past this the result is still right, only
/// longer than it had to be.
const DEADLINE: Duration = Duration::from_secs(2);

/// Two texts lined up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    pub rows: Vec<DiffRow>,
    /// The rows where a run of changes starts.
    pub changes: Vec<u32>,
    /// The most characters on a line.
    pub longest: usize,
}

/// Where each line of `text` is, without its line ending.
fn lines_of(text: &str) -> Vec<(u32, u32)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for piece in text.split_inclusive('\n') {
        let end = start + piece.len();
        let trimmed = piece.trim_end_matches('\n').trim_end_matches('\r');
        lines.push((start as u32, (start + trimmed.len()) as u32));
        start = end;
    }
    lines
}

/// How many bytes `a` and `b` have in common at their starts and, of what is left, at their ends.
fn common_ends(a: &str, b: &str) -> (usize, usize) {
    let prefix: usize =
        a.chars().zip(b.chars()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    let (a, b) = (&a[prefix..], &b[prefix..]);
    let suffix = a.chars().rev().zip(b.chars().rev()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    (prefix, suffix)
}

fn slices<'a>(text: &'a str, lines: &[(u32, u32)]) -> Vec<&'a str> {
    lines.iter().map(|&(start, end)| &text[start as usize..end as usize]).collect()
}

fn line(number: usize, &(start, end): &(u32, u32), changed: Option<(u32, u32)>) -> DiffLine {
    DiffLine { number: number as u32 + 1, start, end, changed }
}

/// Line `left` and `right` up.
pub fn compare(left: &str, right: &str) -> Comparison {
    let (left_lines, right_lines) = (lines_of(left), lines_of(right));
    let (a, b) = (slices(left, &left_lines), slices(right, &right_lines));
    let deadline = Instant::now().checked_add(DEADLINE);
    let ops = capture_diff_slices_deadline(Algorithm::Myers, &a, &b, deadline);

    let mut out = Comparison::default();
    let mut in_change = false;
    for op in ops {
        let (old_index, old_len, new_index, new_len, same) = match op {
            DiffOp::Equal { old_index, new_index, len } => (old_index, len, new_index, len, true),
            DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_len, new_index, 0, false),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, 0, new_index, new_len, false),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_len, new_index, new_len, false),
        };
        if !same && !in_change {
            out.changes.push(out.rows.len() as u32);
        }
        in_change = !same;
        for at in 0..old_len.max(new_len) {
            let old = (at < old_len).then_some(old_index + at);
            let new = (at < new_len).then_some(new_index + at);
            let changed = match (old, new) {
                (Some(old), Some(new)) if !same => {
                    let (prefix, suffix) = common_ends(a[old], b[new]);
                    let part = |&(start, end): &(u32, u32)| (start + prefix as u32, end - suffix as u32);
                    Some((part(&left_lines[old]), part(&right_lines[new])))
                },
                _ => None,
            };
            let left = old.map(|old| line(old, &left_lines[old], changed.map(|parts| parts.0)));
            let right = new.map(|new| line(new, &right_lines[new], changed.map(|parts| parts.1)));
            out.rows.push(DiffRow { left, right, same });
        }
    }
    let count = |lines: &[&str]| lines.iter().map(|line| line.chars().count()).max().unwrap_or(0);
    out.longest = count(&a).max(count(&b));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of<'a>(text: &'a str, line: Option<DiffLine>) -> Option<&'a str> {
        line.map(|line| &text[line.start as usize..line.end as usize])
    }

    #[test]
    fn the_same_text_has_no_changes() {
        let found = compare("a\nb\n", "a\nb\n");
        assert_eq!(found.rows.len(), 2);
        assert!(found.rows.iter().all(|row| row.same));
        assert!(found.changes.is_empty());
    }

    #[test]
    fn an_added_line_has_nothing_across_from_it() {
        let (left, right) = ("a\nc\n", "a\nb\nc\n");
        let found = compare(left, right);
        assert_eq!(found.changes, [1]);
        let row = found.rows[1];
        assert_eq!((row.left, text_of(right, row.right), row.same), (None, Some("b"), false));
        assert_eq!(found.rows[2].left.unwrap().number, 2);
        assert_eq!(found.rows[2].right.unwrap().number, 3);
    }

    #[test]
    fn a_changed_line_marks_only_what_differs() {
        let (left, right) = ("port = 22\n", "port = 2222\n");
        let found = compare(left, right);
        let row = found.rows[0];
        let part = |text: &str, line: DiffLine| {
            let (start, end) = line.changed.unwrap();
            text[start as usize..end as usize].to_owned()
        };
        assert_eq!(part(left, row.left.unwrap()), "");
        assert_eq!(part(right, row.right.unwrap()), "22");
    }

    #[test]
    fn characters_are_never_split() {
        let (left, right) = ("名前=太郎\n", "名前=次郎\n");
        let row = compare(left, right).rows[0];
        let (start, end) = row.left.unwrap().changed.unwrap();
        assert_eq!(&left[start as usize..end as usize], "太");
    }

    #[test]
    fn separate_changes_are_counted_apart_and_a_run_once() {
        let found = compare("a\nb\nc\nd\ne\n", "x\ny\nc\nd\nz\n");
        assert_eq!(found.changes, [0, 4]);
    }

    #[test]
    fn line_endings_and_a_last_newline_are_not_changes() {
        assert!(compare("a\r\nb\r\n", "a\nb").changes.is_empty());
    }

    #[test]
    fn empty_sides_line_up_with_nothing() {
        assert_eq!(compare("", "").rows, []);
        let found = compare("", "a\n");
        assert_eq!((found.rows[0].left, found.changes.as_slice()), (None, [0u32].as_slice()));
    }
}
