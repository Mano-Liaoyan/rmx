//! Windows, tabs and splits as a line of text per item, for the state file.
//!
//! Text the application wrote can still be damaged or come from an older version, so reading it
//! gives up on anything odd instead of guessing: starting with no saved layout is always fine.

use crate::split::{Axis, Split};

/// The first line of a saved session.
const HEADER: &str = "rmx-session 1";

/// A tab as it was left.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedTab<P> {
    pub layout: Split<P>,
    /// Which pane had the keyboard, as an index of [`Split::panes`].
    pub focus: usize,
    pub zoomed: bool,
}

/// A window as it was left.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedWindow<P> {
    pub width: f64,
    pub height: f64,
    pub maximized: bool,
    pub sidebar_open: bool,
    /// Which tab was showing, as an index of `tabs`.
    pub active: usize,
    pub tabs: Vec<SavedTab<P>>,
}

fn flag(value: bool) -> char {
    if value { '1' } else { '0' }
}

/// Write `windows`, each pane as `pane` spells it: on one line, not starting with `split`.
pub fn encode<P>(windows: &[SavedWindow<P>], pane: impl Fn(&P) -> String) -> String {
    fn tree<P>(split: &Split<P>, pane: &impl Fn(&P) -> String, text: &mut String) {
        match split {
            Split::Pane(leaf) => {
                text.push_str("pane ");
                text.push_str(&pane(leaf));
                text.push('\n');
            },
            Split::Pair { axis, ratio, first, second } => {
                let name = if *axis == Axis::Row { "row" } else { "column" };
                text.push_str(&format!("split {name} {ratio}\n"));
                tree(first, pane, text);
                tree(second, pane, text);
            },
        }
    }

    let mut text = format!("{HEADER}\n");
    for window in windows {
        let SavedWindow { width, height, maximized, sidebar_open, active, tabs } = window;
        text.push_str(&format!(
            "window {width} {height} {} {} {active}\n",
            flag(*maximized),
            flag(*sidebar_open),
        ));
        for tab in tabs {
            text.push_str(&format!("tab {} {}\n", flag(tab.zoomed), tab.focus));
            tree(&tab.layout, &pane, &mut text);
        }
    }
    text
}

/// Read what [`encode`] wrote, each pane by `pane`. `None` when it is not all readable.
pub fn decode<P: Copy + PartialEq>(
    text: &str,
    pane: impl Fn(&str) -> Option<P>,
) -> Option<Vec<SavedWindow<P>>> {
    fn tree<'a, P>(
        lines: &mut impl Iterator<Item = &'a str>,
        pane: &impl Fn(&str) -> Option<P>,
        depth: usize,
    ) -> Option<Split<P>> {
        // No tab goes this deep: a file that does is not one of ours.
        if depth > 64 {
            return None;
        }
        let line = lines.next()?;
        if let Some(rest) = line.strip_prefix("pane ") {
            return pane(rest).map(Split::Pane);
        }
        let mut parts = line.strip_prefix("split ")?.split(' ');
        let axis = match parts.next()? {
            "row" => Axis::Row,
            "column" => Axis::Column,
            _ => return None,
        };
        let ratio: f32 = parts.next()?.parse().ok()?;
        if !(0.0..=1.0).contains(&ratio) {
            return None;
        }
        let first = Box::new(tree(lines, pane, depth + 1)?);
        let second = Box::new(tree(lines, pane, depth + 1)?);
        Some(Split::Pair { axis, ratio, first, second })
    }

    let mut lines = text.lines().peekable();
    if lines.next()? != HEADER {
        return None;
    }
    let mut windows = Vec::new();
    while let Some(line) = lines.next() {
        let mut parts = line.strip_prefix("window ")?.split(' ');
        let width: f64 = parts.next()?.parse().ok()?;
        let height: f64 = parts.next()?.parse().ok()?;
        let maximized = parts.next()? == "1";
        let sidebar_open = parts.next()? == "1";
        let active: usize = parts.next()?.parse().ok()?;
        if !(width.is_finite() && height.is_finite() && width > 0. && height > 0.) {
            return None;
        }
        let mut tabs = Vec::new();
        while lines.peek().is_some_and(|line| line.starts_with("tab ")) {
            let mut parts = lines.next()?.strip_prefix("tab ")?.split(' ');
            let zoomed = parts.next()? == "1";
            let focus: usize = parts.next()?.parse().ok()?;
            let layout = tree(&mut lines, &pane, 0)?;
            if focus >= layout.panes().len() {
                return None;
            }
            tabs.push(SavedTab { layout, focus, zoomed });
        }
        if tabs.is_empty() || active >= tabs.len() {
            return None;
        }
        windows.push(SavedWindow { width, height, maximized, sidebar_open, active, tabs });
    }
    Some(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::Side;

    fn sample() -> Vec<SavedWindow<u64>> {
        let layout = Split::Pane(1).join(
            Split::Pane(2).join(Split::Pane(3), Axis::Column, Side::After),
            Axis::Row,
            Side::After,
        );
        vec![
            SavedWindow {
                width: 1280.5,
                height: 800.,
                maximized: false,
                sidebar_open: true,
                active: 1,
                tabs: vec![SavedTab { layout, focus: 2, zoomed: true }, SavedTab {
                    layout: Split::Pane(4),
                    focus: 0,
                    zoomed: false,
                }],
            },
            SavedWindow {
                width: 640.,
                height: 480.,
                maximized: true,
                sidebar_open: false,
                active: 0,
                tabs: vec![SavedTab { layout: Split::Pane(5), focus: 0, zoomed: false }],
            },
        ]
    }

    fn write(windows: &[SavedWindow<u64>]) -> String {
        encode(windows, u64::to_string)
    }

    fn read(text: &str) -> Option<Vec<SavedWindow<u64>>> {
        decode(text, |pane| pane.parse().ok())
    }

    #[test]
    fn a_session_reads_back_as_it_was_written() {
        assert_eq!(read(&write(&sample())), Some(sample()));
    }

    #[test]
    fn ratios_are_kept() {
        let mut windows = sample();
        let Split::Pair { ratio, .. } = &mut windows[0].tabs[0].layout else { panic!() };
        *ratio = 0.3125;
        assert_eq!(read(&write(&windows)), Some(windows));
    }

    #[test]
    fn nothing_saved_reads_as_no_windows() {
        assert_eq!(read(&write(&[])), Some(Vec::new()));
    }

    #[test]
    fn damaged_text_is_given_up_on() {
        let good = write(&sample());
        for bad in [
            String::new(),
            good.replacen("rmx-session 1", "rmx-session 2", 1),
            good.replacen("tab 1 2", "tab 1 9", 1),
            good.replacen("pane 2", "pane two", 1),
            good.replacen("split row 0.5", "split row 7", 1),
            good.replacen("window 1280.5", "window nope", 1),
            good.replacen("split row 0.5", "split diagonal 0.5", 1),
            good.lines().take(4).collect::<Vec<_>>().join("\n"),
        ] {
            assert_eq!(read(&bad), None, "{bad}");
        }
    }

    #[test]
    fn absurdly_deep_trees_are_refused_without_blowing_the_stack() {
        let mut text = format!("{HEADER}\nwindow 800 600 0 0 0\ntab 0 0\n");
        text.push_str(&"split row 0.5\n".repeat(100_000));
        assert_eq!(read(&text), None);
    }
}
