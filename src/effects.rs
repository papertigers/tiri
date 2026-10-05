//! Visual effects over a client's frames, using tachyonfx: new panes
//! fade in from the background.
//!
//! tiri composes frames in its own [`Frame`]; effects run on a copy of it as
//! a ratatui buffer, with colors resolved to RGB through the client's
//! palette so they can be blended. Only cells an effect changed are copied
//! back, so the rest keep the terminal's own default colors. Effects never
//! run while the overview shows kitty thumbnails: those cells' colors
//! identify the image, and changing them would break it.

use std::time::{Duration, Instant};

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Color as RColor, Modifier};
use tachyonfx::{Effect, Interpolation, fx};

use crate::colors::Palette;
use crate::layout::PaneId;
use crate::render::{Color, Frame, Style};

/// How long a new pane takes to fade in.
const OPEN_FADE: Duration = Duration::from_millis(200);

/// What an effect is drawn over, looked up afresh each frame since panes
/// move while the strip scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Pane(PaneId),
}

struct Running {
    anchor: Anchor,
    effect: Effect,
}

/// One client's running effects.
#[derive(Default)]
pub struct Effects {
    running: Vec<Running>,
    last: Option<Instant>,
}

impl Effects {
    /// A newly opened pane fades in, its text and colors rising out of
    /// the background.
    pub fn pane_opened(&mut self, pane: PaneId, palette: &Palette) {
        let [r, g, b] = palette.background;
        let background = RColor::Rgb(r, g, b);
        self.add(
            Anchor::Pane(pane),
            fx::fade_from(background, background, (OPEN_FADE, Interpolation::QuadOut)),
        );
    }

    fn add(&mut self, anchor: Anchor, effect: Effect) {
        self.running.push(Running { anchor, effect });
    }

    pub fn is_active(&self) -> bool {
        !self.running.is_empty()
    }

    pub fn clear(&mut self) {
        self.running.clear();
        self.last = None;
    }

    /// What each running effect is drawn over.
    pub fn anchors(&self) -> Vec<Anchor> {
        self.running.iter().map(|r| r.anchor).collect()
    }

    /// Advances the effects to `now` and draws them over `frame`. `areas`
    /// gives where each anchor is on screen now, if it's visible; effects
    /// whose anchor has gone away are dropped.
    pub fn apply(
        &mut self,
        frame: &mut Frame,
        palette: &Palette,
        now: Instant,
        areas: &[(Anchor, Option<Rect>)],
    ) {
        if self.running.is_empty() {
            self.last = None;
            return;
        }
        let elapsed = self.last.map_or(Duration::ZERO, |last| now - last);
        self.last = Some(now);

        let screen = Rect::new(0, 0, frame.width(), frame.height());
        let original = to_buffer(frame, palette);
        let mut buffer = original.clone();
        self.running.retain_mut(|running| {
            let area = areas
                .iter()
                .find(|(anchor, _)| *anchor == running.anchor)
                .and_then(|(_, area)| *area);
            let Some(area) = area else {
                return false;
            };
            running.effect.set_area(area.intersection(screen));
            running.effect.process(elapsed, &mut buffer, screen);
            !running.effect.done()
        });
        if self.running.is_empty() {
            // Otherwise the next effect would count the idle time since
            // this one as already elapsed, and finish at once.
            self.last = None;
        }
        copy_changes(frame, &original, &buffer);
    }
}

/// `frame` as a ratatui buffer, colors resolved to RGB.
fn to_buffer(frame: &Frame, palette: &Palette) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, frame.width(), frame.height()));
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let (sym, _, style) = frame.content(x, y);
            let cell = &mut buffer[(x, y)];
            cell.set_symbol(sym);
            cell.fg = rgb(style.fg, palette, false);
            cell.bg = rgb(style.bg, palette, true);
            cell.modifier = modifiers(style);
        }
    }
    buffer
}

/// Copies back the cells effects changed.
fn copy_changes(frame: &mut Frame, original: &Buffer, changed: &Buffer) {
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let after = &changed[(x, y)];
            if *after == original[(x, y)] {
                continue;
            }
            let (_, wide, mut style) = frame.content(x, y);
            style.fg = color(after.fg);
            style.bg = color(after.bg);
            let (x, y, sym) = (i32::from(x), i32::from(y), after.symbol().to_owned());
            if sym.is_empty() {
                // The covered half of a wide character: only its colors change.
                continue;
            }
            if wide && sym == frame.content(x as u16, y as u16).0 {
                frame.put_wide(x, y, &sym, style);
            } else {
                frame.put(x, y, &sym, style);
            }
        }
    }
}

fn rgb(color: Color, palette: &Palette, background: bool) -> RColor {
    let [r, g, b] = match color {
        Color::Default if background => palette.background,
        Color::Default => palette.foreground,
        Color::Idx(i) => palette.indexed(i),
        Color::Rgb(r, g, b) => [r, g, b],
    };
    RColor::Rgb(r, g, b)
}

fn color(color: RColor) -> Color {
    match color {
        RColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
        RColor::Indexed(i) => Color::Idx(i),
        _ => Color::Default,
    }
}

fn modifiers(style: Style) -> Modifier {
    let mut m = Modifier::empty();
    for (on, flag) in [
        (style.bold, Modifier::BOLD),
        (style.dim, Modifier::DIM),
        (style.italic, Modifier::ITALIC),
        (style.underline, Modifier::UNDERLINED),
        (style.inverse, Modifier::REVERSED),
        (style.strikeout, Modifier::CROSSED_OUT),
    ] {
        if on {
            m |= flag;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with_text() -> Frame {
        let mut frame = Frame::new(10, 3);
        frame.put_str(1, 1, "hello", Style::default());
        frame
    }

    #[test]
    fn untouched_cells_keep_their_default_colors() {
        let mut frame = frame_with_text();
        let mut effects = Effects::default();
        effects.pane_opened(PaneId(0), &Palette::default());
        let start = Instant::now();
        // An effect over an area away from the text.
        let areas = [(Anchor::Pane(PaneId(0)), Some(Rect::new(7, 0, 3, 1)))];
        effects.apply(&mut frame, &Palette::default(), start, &areas);
        effects.apply(
            &mut frame,
            &Palette::default(),
            start + Duration::from_millis(100),
            &areas,
        );
        let (sym, _, style) = frame.content(2, 1);
        assert_eq!(
            (sym, style.fg, style.bg),
            ("e", Color::Default, Color::Default)
        );
    }

    #[test]
    fn opening_fades_in_from_the_background() {
        let palette = Palette::default();
        let mut effects = Effects::default();
        effects.pane_opened(PaneId(0), &palette);
        let areas = [(Anchor::Pane(PaneId(0)), Some(Rect::new(0, 0, 10, 3)))];
        let start = Instant::now();

        let mut frame = frame_with_text();
        effects.apply(&mut frame, &palette, start, &areas);
        let (sym, _, style) = frame.content(2, 1);
        let [r, g, b] = palette.background;
        assert_eq!(
            (sym, style.fg),
            ("e", Color::Rgb(r, g, b)),
            "text starts as background"
        );

        let mut frame = frame_with_text();
        effects.apply(&mut frame, &palette, start + OPEN_FADE * 2, &areas);
        let (_, _, style) = frame.content(2, 1);
        assert_eq!(style.fg, Color::Default, "and ends as it was");
        assert!(!effects.is_active(), "finished effects are dropped");
    }

    #[test]
    fn effects_whose_pane_went_away_are_dropped() {
        let mut frame = frame_with_text();
        let mut effects = Effects::default();
        effects.pane_opened(PaneId(0), &Palette::default());
        effects.apply(
            &mut frame,
            &Palette::default(),
            Instant::now(),
            &[(Anchor::Pane(PaneId(0)), None)],
        );
        assert!(!effects.is_active());
    }

    #[test]
    fn a_later_effect_starts_from_the_beginning() {
        let palette = Palette::default();
        let areas = [(Anchor::Pane(PaneId(0)), Some(Rect::new(0, 0, 10, 3)))];
        let mut effects = Effects::default();
        let start = Instant::now();
        effects.pane_opened(PaneId(0), &palette);
        effects.apply(&mut frame_with_text(), &palette, start, &areas);
        effects.apply(
            &mut frame_with_text(),
            &palette,
            start + OPEN_FADE * 2,
            &areas,
        );
        assert!(!effects.is_active());

        // A second pane opens much later; its fade still starts at the start.
        effects.pane_opened(PaneId(0), &palette);
        let mut frame = frame_with_text();
        effects.apply(&mut frame, &palette, start + Duration::from_secs(5), &areas);
        let [r, g, b] = palette.background;
        assert_eq!(frame.content(2, 1).2.fg, Color::Rgb(r, g, b));
        assert!(effects.is_active());
    }
}
