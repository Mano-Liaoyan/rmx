//! Panels resized by dragging: their sizes, and the handle on their edges.
//!
//! A handle shows a pill under the pointer as it is hovered, which grows while it is dragged, and
//! a double click gives the panel its default size again.

use std::time::Instant;

use egui::{CursorIcon, Id, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use rmx_layout::anim::{Curve, Tween};

use crate::draw::palette;
use crate::theme::{Palette, SIDEBAR_WIDTH};

/// Thickness of the pill and its length at rest, at most a quarter of the edge.
const PILL_WIDTH: f32 = 4.;
const PILL_LENGTH: f32 = 40.;
/// How much longer it grows while dragged.
const PILL_GROWTH: f32 = 1.6;
/// How long it takes to show, hide or grow, in seconds.
const PILL_SECS: f32 = 0.14;

/// The default sizes of a file pane's parts, in points.
pub const FILES_SPLIT: f32 = 0.5;
pub const FILES_QUEUE: f32 = 148.;
pub const FILES_SIZE: f32 = 76.;
pub const FILES_DATE: f32 = 112.;
/// The default width of the list of the snippets dialog.
pub const SNIPPETS_LIST: f32 = 248.;

/// The sizes the user gave the panels, in points, for the state file of this device.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panels {
    pub sidebar: f32,
    /// How much of a file pane's width its first column takes, from 0 to 1.
    pub files_split: f32,
    pub files_queue: f32,
    pub files_size: f32,
    pub files_date: f32,
    /// Whether the queue is folded down to its bar.
    pub files_queue_folded: bool,
    /// Sizes of dialogs, 0 being their defaults.
    pub settings: [f32; 2],
    pub snippets: [f32; 2],
    pub snippets_list: f32,
}

impl Default for Panels {
    fn default() -> Self {
        Self {
            sidebar: SIDEBAR_WIDTH,
            files_split: FILES_SPLIT,
            files_queue: FILES_QUEUE,
            files_size: FILES_SIZE,
            files_date: FILES_DATE,
            files_queue_folded: false,
            settings: [0.; 2],
            snippets: [0.; 2],
            snippets_list: SNIPPETS_LIST,
        }
    }
}

impl Panels {
    /// One line for the state file.
    pub fn spell(&self) -> String {
        let [sw, sh] = self.settings;
        let [nw, nh] = self.snippets;
        format!(
            "{} {} {} {} {} {sw} {sh} {nw} {nh} {} {}",
            self.sidebar,
            self.files_split,
            self.files_queue,
            self.files_size,
            self.files_date,
            self.snippets_list,
            u8::from(self.files_queue_folded),
        )
    }

    /// What [`Panels::spell`] wrote, or the defaults for what is damaged or was written by another
    /// version.
    pub fn read(text: &str) -> Self {
        let values: Vec<f32> = text.split(' ').filter_map(|part| part.parse().ok()).collect();
        // A file from before the queue could be folded has one value fewer.
        let (
            Some(
                &[
                    sidebar,
                    files_split,
                    files_queue,
                    files_size,
                    files_date,
                    sw,
                    sh,
                    nw,
                    nh,
                    snippets_list,
                ],
            ),
            true,
        ) = (values.first_chunk::<10>(), values.len() <= 11)
        else {
            return Self::default();
        };
        let folded = values.get(10).is_some_and(|value| *value != 0.);
        if values.iter().any(|value| !value.is_finite() || *value < 0.) {
            return Self::default();
        }
        Self {
            sidebar,
            files_split,
            files_queue,
            files_size,
            files_date,
            files_queue_folded: folded,
            settings: [sw, sh],
            snippets: [nw, nh],
            snippets_list,
        }
    }
}

/// What became of a handle in this pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct Handle {
    /// Where the edge is being dragged to.
    pub to: Option<f32>,
    /// The button went up.
    pub released: bool,
    /// Double clicked: back to the default.
    pub reset: bool,
}

/// Draw the pill of a handle along `band` at `along`, `level` shown: from 0 hidden, through 1
/// hovered, to 2 dragged.
pub fn paint_pill(
    painter: &egui::Painter,
    band: Rect,
    vertical: bool,
    along: f32,
    level: f32,
    p: &Palette,
) {
    if level <= 0. {
        return;
    }
    let grow = (level - 1.).clamp(0., 1.);
    let (start, span) =
        if vertical { (band.top(), band.height()) } else { (band.left(), band.width()) };
    let length = PILL_LENGTH.min(span / 4.) * (1. + (PILL_GROWTH - 1.) * grow);
    let middle = along.clamp(start + length / 2., start + span - length / 2.);
    let center = band.center();
    let rect = if vertical {
        Rect::from_center_size(pos2(center.x, middle), vec2(PILL_WIDTH, length))
    } else {
        Rect::from_center_size(pos2(middle, center.y), vec2(length, PILL_WIDTH))
    };
    let rest = p.border.lerp_to_gamma(p.foreground, 0.4);
    let color = rest.lerp_to_gamma(p.foreground, grow).gamma_multiply(level.min(1.));
    painter.rect_filled(rect, 2, color);
}

/// A handle along `band`, dragging an edge at `edge` across it: `vertical` is the direction of the
/// edge, so a vertical one moves sideways.
pub fn resize_handle(ui: &mut Ui, band: Rect, id: Id, vertical: bool, edge: f32) -> Handle {
    let response = ui.interact(band, id, Sense::click_and_drag());
    let hot = response.hovered() || response.dragged();
    if hot {
        ui.ctx().set_cursor_icon(if vertical {
            CursorIcon::ResizeHorizontal
        } else {
            CursorIcon::ResizeVertical
        });
    }

    let across = |pos: Pos2| if vertical { pos.x } else { pos.y };
    let along = |pos: Pos2| if vertical { pos.y } else { pos.x };
    let ctx = ui.ctx().clone();
    let (grab_id, along_id) = (id.with("grab"), id.with("along"));
    if response.drag_started() {
        let origin = ctx.input(|i| i.pointer.press_origin());
        let grab = origin.map_or(0., |origin| across(origin) - edge);
        ctx.data_mut(|data| data.insert_temp(grab_id, grab));
    }
    let to = response
        .interact_pointer_pos()
        .filter(|_| response.dragged())
        .map(|pos| across(pos) - ctx.data(|data| data.get_temp::<f32>(grab_id)).unwrap_or(0.));

    if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()).filter(|_| hot) {
        ctx.data_mut(|data| data.insert_temp(along_id, along(pos)));
    }
    let target = if response.dragged() {
        2.
    } else if hot {
        1.
    } else {
        0.
    };
    let level = ctx.animate_value_with_time(id.with("pill"), target, PILL_SECS);
    if level > 0. {
        let eased = if level <= 1. { Curve::EASE_OUT.at(level) } else { level };
        let at = ctx.data(|data| data.get_temp::<f32>(along_id)).unwrap_or(band.center().y);
        let p = palette(ui);
        paint_pill(ui.painter(), band, vertical, at, eased, p);
    }
    Handle { to, released: response.drag_stopped(), reset: response.double_clicked() }
}

/// A hairline, for separating groups without a caption.
pub fn group_rule(ui: &mut Ui) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1., p.border));
}

/// How long the controls of a bar take to appear, stay once the pointer left them, and fade.
const REVEAL_MS: u64 = 140;
const LINGER_MS: u64 = 350;
const HIDE_MS: u64 = 180;

/// How visible controls shown under the pointer are: they appear as it nears them, and stay a
/// moment after it left before they fade.
#[derive(Clone, Copy, Debug)]
pub struct Reveal {
    level: Tween<f32>,
    /// When the pointer left them.
    left_at: Option<Instant>,
}

impl Default for Reveal {
    fn default() -> Self {
        Self { level: Tween::new(0.), left_at: None }
    }
}

impl Reveal {
    /// How visible the controls are at `now`, moving to whether the pointer is `near`.
    pub fn level(&mut self, near: bool, now: Instant) -> f32 {
        if near {
            self.left_at = None;
            self.level.to(1., now, REVEAL_MS, Curve::EASE_OUT);
        } else if self.left_at.is_none() && self.level.get(now) > 0. {
            self.left_at = Some(now);
        }
        if let Some(left) = self.left_at.map(|left| left + linger())
            && now >= left
        {
            self.level.to(0., left, HIDE_MS, Curve::EASE_OUT);
        }
        self.level.get(now)
    }

    /// When to look again, none when at rest: at once while it moves, or when the wait is over.
    pub fn wake(&self, now: Instant) -> Option<Instant> {
        if self.level.running(now) {
            return Some(now);
        }
        self.left_at.map(|left| left + linger()).filter(|&at| at > now)
    }

    /// Hide at once.
    pub fn hide(&mut self) {
        *self = Self::default();
    }
}

fn linger() -> std::time::Duration {
    std::time::Duration::from_millis(LINGER_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sizes_read_back_and_damage_means_the_defaults() {
        let panels = Panels {
            sidebar: 300.5,
            files_split: 0.25,
            settings: [640., 480.],
            ..Panels::default()
        };
        assert_eq!(Panels::read(&panels.spell()), panels);
        assert_eq!(Panels::read("nonsense"), Panels::default());
        assert_eq!(Panels::read("1 2 3"), Panels::default());
        assert_eq!(Panels::read("NaN 0 0 0 0 0 0 0 0 0"), Panels::default());
        let folded = Panels { files_queue_folded: true, ..Panels::default() };
        assert_eq!(Panels::read(&folded.spell()), folded);
        let before_folding = Panels { sidebar: 300., ..Panels::default() };
        let old = before_folding.spell();
        assert_eq!(
            Panels::read(old.rsplit_once(' ').unwrap().0),
            before_folding,
            "older files still read"
        );
    }

    #[test]
    fn controls_appear_stay_a_while_and_fade() {
        let now = Instant::now();
        let ms = |ms: u64| Duration::from_millis(ms);
        let mut reveal = Reveal::default();
        assert_eq!(reveal.level(false, now), 0.);
        assert_eq!(reveal.wake(now), None, "at rest nothing wakes the application");

        reveal.level(true, now);
        let shown = now + ms(REVEAL_MS + 1);
        assert_eq!(reveal.level(true, shown), 1.);
        assert_eq!(reveal.wake(shown), None);

        assert_eq!(reveal.level(false, shown), 1.);
        let linger = reveal.wake(shown).expect("waiting to fade");
        assert_eq!(linger, shown + ms(LINGER_MS));
        assert_eq!(reveal.level(false, shown + ms(LINGER_MS - 1)), 1.);

        let gone = shown + ms(LINGER_MS + HIDE_MS + 1);
        assert_eq!(reveal.level(false, gone), 0.);
        assert_eq!(reveal.wake(gone), None);
    }
}
