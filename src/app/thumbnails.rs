// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Overview thumbnails for terminals with kitty graphics: drawn from each
//! pane's screen, uploaded once, re-uploaded as the pane changes or as the
//! overview fades.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::kitty;
use crate::layout::PaneId;
use crate::pane::Pane;
use crate::thumbnail;

use super::{App, Client};

/// Kitty image ids for overview thumbnails are this plus the pane id.
const THUMBNAIL_ID_BASE: u32 = 0x74_0000;

/// The kitty image id of pane `id`'s thumbnail.
pub(super) fn image_id(id: PaneId) -> u32 {
    THUMBNAIL_ID_BASE + id.0
}

/// Thumbnails of busy panes are redrawn at most this often.
pub(super) const THUMBNAIL_INTERVAL: Duration = Duration::from_millis(250);
/// Fading a thumbnail in uploads it this many times, at rising opacity.
const OPACITY_STEPS: u8 = 4;

/// An overview thumbnail uploaded to a client's terminal.
pub(super) struct Thumbnail {
    /// Placement size in cells.
    pub(super) size: (u16, u16),
    /// The pane's generation when this was drawn.
    pub(super) generation: u64,
    pub(super) uploaded: Instant,
    /// The image as drawn, kept for fading it in while the overview is
    /// open. The terminal keeps its own copy after it closes.
    image: Option<thumbnail::Image>,
    /// The opacity it was last uploaded at, in steps of [`OPACITY_STEPS`];
    /// None until it's been uploaded.
    opacity: Option<u8>,
}

impl Thumbnail {
    /// Shows this as image `image_id`: uploads it if it's not already there
    /// at `opacity` (rounded to a few steps), and places it at `size` cells
    /// if it isn't already.
    fn show(
        &mut self,
        escapes: &mut Vec<u8>,
        image_id: u32,
        size: (u16, u16),
        opacity: f32,
    ) {
        let step = opacity_step(opacity);
        let uploading = self.opacity != Some(step);
        if uploading {
            // Only a thumbnail parked with the overview closed has no image,
            // and it's drawn again before it needs one.
            let Some(image) = &self.image else {
                return;
            };
            if step == OPACITY_STEPS {
                kitty::transmit(escapes, image_id, image);
            } else {
                let faded = image
                    .with_opacity(f32::from(step) / f32::from(OPACITY_STEPS));
                kitty::transmit(escapes, image_id, &faded);
            }
            self.opacity = Some(step);
        }
        if uploading || self.size != size {
            kitty::place(escapes, image_id, size.0, size.1);
            self.size = size;
        }
    }
}

/// `opacity`, 0 to 1, as one of the few steps thumbnails are uploaded at.
fn opacity_step(opacity: f32) -> u8 {
    (opacity.clamp(0.0, 1.0) * f32::from(OPACITY_STEPS)).round() as u8
}

impl App {
    /// Thumbnails show in the overview, if the client's terminal can.
    pub(super) fn showing_thumbnails(&self, client: &Client) -> bool {
        client.kitty_overview && self.workspaces.in_overview(client.id)
    }
}

impl Client {
    pub(super) fn clear_thumbnails(&mut self) {
        for (id, _) in self.thumbnails.drain() {
            kitty::delete(&mut self.escapes, image_id(id));
        }
    }

    /// Keeps the thumbnails of panes that still exist in the terminal after
    /// the overview closes, so opening it again only uploads those that
    /// changed meanwhile. Their images can go: the terminal has them.
    pub(super) fn park_thumbnails(&mut self, panes: &HashMap<PaneId, Pane>) {
        self.retain_thumbnails(|id| panes.contains_key(id));
        for thumb in self.thumbnails.values_mut() {
            thumb.image = None;
        }
    }

    /// Frees the thumbnails of panes for which `keep` is false.
    pub(super) fn retain_thumbnails(&mut self, keep: impl Fn(&PaneId) -> bool) {
        let escapes = &mut self.escapes;
        self.thumbnails.retain(|id, _| {
            let kept = keep(id);
            if !kept {
                kitty::delete(escapes, image_id(*id));
            }
            kept
        });
    }

    /// Keeps `id`'s thumbnail current: draws it if there's none yet or the
    /// pane changed (at most every [`THUMBNAIL_INTERVAL`]), uploads it at
    /// `opacity` (0 to 1, in a few steps, for fading it in), and sizes its
    /// placement to `size` cells.
    pub(super) fn refresh_thumbnail(
        &mut self,
        panes: &HashMap<PaneId, Pane>,
        id: PaneId,
        size: (u16, u16),
        opacity: f32,
        now: Instant,
    ) {
        let Some(pane) = panes.get(&id) else {
            return;
        };
        let image_id = image_id(id);
        let generation = pane.generation();
        let stale = self.thumbnails.get(&id).is_none_or(|t| {
            (t.generation != generation && now >= t.uploaded + THUMBNAIL_INTERVAL)
                // Parked, and wanted at another opacity than it was left at.
                || (t.image.is_none() && t.opacity != Some(opacity_step(opacity)))
        });
        if stale {
            let image = thumbnail::rasterize(
                pane.term(),
                &self.palette,
                self.thumbnail_cell,
            );
            let Some(image) = image else {
                // Too big for a thumbnail; the overview shows its text.
                if self.thumbnails.remove(&id).is_some() {
                    kitty::delete(&mut self.escapes, image_id);
                }
                return;
            };
            self.thumbnails.insert(
                id,
                Thumbnail {
                    size,
                    generation,
                    uploaded: now,
                    image: Some(image),
                    opacity: None,
                },
            );
        }
        let thumb = self.thumbnails.get_mut(&id).expect("inserted if missing");
        thumb.show(&mut self.escapes, image_id, size, opacity);
    }

    /// Re-uploads the thumbnails already shown at `opacity`, as the overview
    /// fades out.
    pub(super) fn fade_thumbnails(&mut self, opacity: f32) {
        for (id, thumb) in &mut self.thumbnails {
            let size = thumb.size;
            thumb.show(&mut self.escapes, image_id(*id), size, opacity);
        }
    }

    /// The inner size of a pane's box in cells, as a thumbnail placement.
    pub(super) fn thumbnail_size(w: i32, h: i32) -> (u16, u16) {
        let clamp = |n: i32| {
            u16::try_from(n.max(1))
                .map_or(kitty::MAX_CELLS, |n| n.min(kitty::MAX_CELLS))
        };
        (clamp(w - 2), clamp(h - 2))
    }
}
