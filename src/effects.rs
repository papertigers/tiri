// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Visual effects over a client's frames, using tachyonfx: new panes fade
//! in from the background, and the overview fades in and out.
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
use tachyonfx::{ColorSpace, Effect, Interpolation, fx};

use crate::colors::Palette;
use crate::kitty;
use crate::layout::PaneId;
use crate::render::{Color, Frame, Style};

/// How fades blend colors: straight toward the background, so red text
/// fades through darker reds. tachyonfx's default, HSL, turns the hue on
/// the way, and red text passes through purple to reach a dark blue.
const FADE_COLORS: ColorSpace = ColorSpace::Rgb;

/// How long a new pane takes to fade in.
const OPEN_FADE: Duration = Duration::from_millis(200);

/// The overview transition's phases: the old view going out, then the new
/// one coming in.
const OVERVIEW_OUT: Duration = Duration::from_millis(130);
const OVERVIEW_IN: Duration = Duration::from_millis(260);

struct Running {
    /// What the effect is drawn over, looked up afresh each frame since
    /// panes move while the strip scrolls.
    pane: PaneId,
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
        let background = background(palette);
        let effect = fx::fade_from(
            background,
            background,
            (OPEN_FADE, Interpolation::QuadOut),
        )
        .with_color_space(FADE_COLORS);
        self.running.push(Running { pane, effect });
    }

    pub fn is_active(&self) -> bool {
        !self.running.is_empty()
    }

    pub fn clear(&mut self) {
        self.running.clear();
        self.last = None;
    }

    /// The panes running effects are drawn over.
    pub fn panes(&self) -> Vec<PaneId> {
        self.running.iter().map(|r| r.pane).collect()
    }

    /// Advances the effects to `now` and draws them over `frame`. `areas`
    /// gives where each pane is on screen now, if it's visible; effects
    /// whose pane has gone away are dropped.
    pub fn apply(
        &mut self,
        frame: &mut Frame,
        palette: &Palette,
        now: Instant,
        areas: &[(PaneId, Option<Rect>)],
    ) {
        if self.running.is_empty() {
            self.last = None;
            return;
        }
        let elapsed = self.last.map_or(Duration::ZERO, |last| now - last);
        self.last = Some(now);

        let screen = Rect::new(0, 0, frame.width(), frame.height());
        on_buffer(frame, palette, |buffer| {
            self.running.retain_mut(|running| {
                let area = areas
                    .iter()
                    .find(|(pane, _)| *pane == running.pane)
                    .and_then(|(_, area)| *area);
                let Some(area) = area else {
                    return false;
                };
                running.effect.set_area(area.intersection(screen));
                running.effect.process(elapsed, buffer, screen);
                !running.effect.done()
            });
        });
        if self.running.is_empty() {
            // Otherwise the next effect would count the idle time since
            // this one as already elapsed, and finish at once.
            self.last = None;
        }
    }
}

/// Switching between the normal view and the overview in two phases: the
/// old view fades out to the background, then the new one, already at its
/// final size, fades in from it.
///
/// Text fades through tachyonfx. Thumbnails can't be recolored, since a
/// cell's color is what names its image, so they fade by being uploaded at
/// changing opacity instead, following [`Transition::image_opacity`].
pub struct Transition {
    /// The view going out, as last drawn.
    from: Frame,
    out: Effect,
    into: Effect,
    /// Opening the overview, rather than closing it.
    opening: bool,
    coming_in: bool,
    last: Option<Instant>,
    /// How long each phase has been running.
    out_elapsed: Duration,
    in_elapsed: Duration,
}

impl Transition {
    /// Into the overview if `opening`, otherwise out of it, from `from`, the
    /// view as last drawn.
    pub fn new(from: Frame, opening: bool, palette: &Palette) -> Self {
        let background = background(palette);
        Self {
            from,
            out: fx::fade_to(
                background,
                background,
                (OVERVIEW_OUT, Interpolation::QuadIn),
            )
            .with_color_space(FADE_COLORS),
            into: fx::fade_from(
                background,
                background,
                (OVERVIEW_IN, Interpolation::QuadOut),
            )
            .with_color_space(FADE_COLORS),
            opening,
            coming_in: false,
            last: None,
            out_elapsed: Duration::ZERO,
            in_elapsed: Duration::ZERO,
        }
    }

    /// How opaque the overview's thumbnails should be right now, following
    /// the text: rising as the overview fades in, falling as it fades out.
    pub fn image_opacity(&self) -> f32 {
        let progress = |elapsed: Duration, length: Duration| {
            (elapsed.as_secs_f32() / length.as_secs_f32()).min(1.0)
        };
        match (self.opening, self.coming_in) {
            (true, false) | (false, true) => 0.0,
            (true, true) => {
                // Matching the text's QuadOut fade in.
                let t = progress(self.in_elapsed, OVERVIEW_IN);
                1.0 - (1.0 - t) * (1.0 - t)
            }
            (false, false) => {
                // Matching the text's QuadIn fade out.
                let t = progress(self.out_elapsed, OVERVIEW_OUT);
                1.0 - t * t
            }
        }
    }

    /// Draws the transition at `now` into `frame`, which holds the new
    /// view. The status bar's row is left alone. Returns false once done.
    pub fn apply(
        &mut self,
        frame: &mut Frame,
        palette: &Palette,
        now: Instant,
    ) -> bool {
        let elapsed = self.last.map_or(Duration::ZERO, |last| now - last);
        self.last = Some(now);
        let area =
            Rect::new(0, 0, frame.width(), frame.height().saturating_sub(1));
        if !self.coming_in
            && (self.from.width(), self.from.height())
                != (frame.width(), frame.height())
        {
            // The terminal changed size, so the old view no longer fits it.
            // Skip to bringing the new one in.
            self.coming_in = true;
        }
        if !self.coming_in {
            self.out_elapsed += elapsed;
            // The old view, out of date though it is, still fills the screen.
            let mut old = self.from.clone();
            on_buffer(&mut old, palette, |buffer| {
                self.out.process(elapsed, buffer, area);
            });
            let status = frame.height().saturating_sub(1);
            for x in 0..frame.width() {
                // Keep the new status bar.
                let (sym, wide, style) = frame.content(x, status);
                let sym = sym.to_owned();
                if wide {
                    old.put_wide(i32::from(x), i32::from(status), &sym, style);
                } else if !sym.is_empty() {
                    old.put(i32::from(x), i32::from(status), &sym, style);
                }
            }
            *frame = old;
            if !self.out.done() {
                return true;
            }
            self.coming_in = true;
            self.last = None;
            return true;
        }
        self.in_elapsed += elapsed;
        on_buffer(frame, palette, |buffer| {
            self.into.process(elapsed, buffer, area);
        });
        !self.into.done()
    }
}

fn background(palette: &Palette) -> RColor {
    let [r, g, b] = palette.background;
    RColor::Rgb(r, g, b)
}

/// Runs `f` over `frame` as a ratatui buffer, then copies back the cells
/// it changed.
fn on_buffer(
    frame: &mut Frame,
    palette: &Palette,
    f: impl FnOnce(&mut Buffer),
) {
    let original = to_buffer(frame, palette);
    let mut buffer = original.clone();
    f(&mut buffer);
    copy_changes(frame, &original, &buffer);
}

/// `frame` as a ratatui buffer, colors resolved to RGB.
fn to_buffer(frame: &Frame, palette: &Palette) -> Buffer {
    let mut buffer =
        Buffer::empty(Rect::new(0, 0, frame.width(), frame.height()));
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

/// Copies back the cells effects changed. Kitty placeholder cells keep
/// their colors whatever an effect did, since their color names the image
/// they show; an effect may still blank them.
fn copy_changes(frame: &mut Frame, original: &Buffer, changed: &Buffer) {
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let after = &changed[(x, y)];
            let before = &original[(x, y)];
            if *after == *before {
                continue;
            }
            let placeholder = before.symbol().starts_with(kitty::PLACEHOLDER);
            if placeholder && after.symbol() == before.symbol() {
                continue;
            }
            let (_, wide, mut style) = frame.content(x, y);
            style.fg = color(after.fg);
            style.bg = color(after.bg);
            let (x, y, sym) =
                (i32::from(x), i32::from(y), after.symbol().to_owned());
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
        let areas = [(PaneId(0), Some(Rect::new(7, 0, 3, 1)))];
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
        let areas = [(PaneId(0), Some(Rect::new(0, 0, 10, 3)))];
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
            &[(PaneId(0), None)],
        );
        assert!(!effects.is_active());
    }

    #[test]
    fn a_later_effect_starts_from_the_beginning() {
        let palette = Palette::default();
        let areas = [(PaneId(0), Some(Rect::new(0, 0, 10, 3)))];
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
        effects.apply(
            &mut frame,
            &palette,
            start + Duration::from_secs(5),
            &areas,
        );
        let [r, g, b] = palette.background;
        assert_eq!(frame.content(2, 1).2.fg, Color::Rgb(r, g, b));
        assert!(effects.is_active());
    }

    /// A frame filled with `ch`, as a stand-in for a view.
    fn filled(ch: &str) -> Frame {
        let mut frame = Frame::new(40, 11);
        for y in 0..10 {
            frame.put_str(0, y, &ch.repeat(40), Style::default());
        }
        frame
    }

    /// Runs `transition` to `at` after it started, with `new` as the view
    /// coming in, and returns what's drawn.
    fn run(transition: &mut Transition, new: &Frame, at: &[Duration]) -> Frame {
        let start = Instant::now();
        let mut frame = new.clone();
        for &t in at {
            frame = new.clone();
            transition.apply(&mut frame, &Palette::default(), start + t);
        }
        frame
    }

    #[test]
    fn opening_fades_the_overview_in() {
        let mut transition =
            Transition::new(filled("o"), true, &Palette::default());
        let ms = Duration::from_millis;
        assert_eq!(transition.image_opacity(), 0.0);
        // Through the fade-out, then part way into the fade-in.
        let frame = run(
            &mut transition,
            &filled("n"),
            &[ms(0), ms(140), ms(141), ms(141 + 100)],
        );
        let (sym, _, style) = frame.content(0, 0);
        assert_eq!(sym, "n", "nothing blanked, just fading");
        assert_ne!(style.fg, Color::Default, "still mid-fade");
        let opacity = transition.image_opacity();
        assert!(opacity > 0.0 && opacity < 1.0, "{opacity}");
    }

    #[test]
    fn closing_fades_the_overview_out() {
        let mut transition =
            Transition::new(filled("o"), false, &Palette::default());
        let ms = Duration::from_millis;
        assert_eq!(transition.image_opacity(), 1.0);
        let frame = run(&mut transition, &filled("n"), &[ms(0), ms(80)]);
        let (sym, _, style) = frame.content(0, 0);
        assert_eq!(sym, "o", "the overview, fading");
        assert_ne!(style.fg, Color::Default);
        let opacity = transition.image_opacity();
        assert!(opacity > 0.0 && opacity < 1.0, "{opacity}");
        // Once the view is coming back in, the thumbnails are gone.
        run(&mut transition, &filled("n"), &[ms(140), ms(141)]);
        assert_eq!(transition.image_opacity(), 0.0);
    }

    #[test]
    fn placeholder_cells_keep_their_colors() {
        let mut new = filled("n");
        let id = Style {
            fg: Color::Rgb(0, 0, 7),
            underline_color: Color::Rgb(0, 0, 7),
            ..Style::default()
        };
        new.put(5, 5, &kitty::placeholder(0, 0), id);
        let mut transition =
            Transition::new(filled("o"), true, &Palette::default());
        let ms = Duration::from_millis;
        let frame = run(
            &mut transition,
            &new,
            &[ms(0), ms(140), ms(141), ms(141 + 100)],
        );
        assert_eq!(frame.content(5, 5).2.fg, Color::Rgb(0, 0, 7));
        assert_eq!(frame.content(5, 5).2.underline_color, Color::Rgb(0, 0, 7));
    }

    #[test]
    fn a_transition_ends_showing_the_new_view_untouched() {
        let mut transition =
            Transition::new(filled("o"), true, &Palette::default());
        let ms = Duration::from_millis;
        let start = Instant::now();
        let new = filled("n");
        let mut running = true;
        let mut frame = new.clone();
        for t in (0..60).map(|i| ms(i * 16)) {
            frame = new.clone();
            running =
                transition.apply(&mut frame, &Palette::default(), start + t);
            if !running {
                break;
            }
        }
        assert!(!running);
        assert_eq!(frame.content(0, 0), new.content(0, 0));
        assert_eq!(frame.content(20, 5), new.content(20, 5));
    }

    #[test]
    fn the_status_bar_is_never_part_of_it() {
        let mut transition =
            Transition::new(filled("o"), false, &Palette::default());
        let mut new = filled("n");
        new.put_str(0, 10, "status", Style::default());
        let frame = run(
            &mut transition,
            &new,
            &[Duration::ZERO, Duration::from_millis(60)],
        );
        assert_eq!(frame.content(0, 10).0, "s");
        assert_eq!(frame.content(0, 10).2.fg, Color::Default);
    }
}
