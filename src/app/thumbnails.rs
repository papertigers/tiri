// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Overview thumbnails for terminals with kitty graphics: drawn from each
//! pane's screen, uploaded once, re-uploaded as the pane changes or as the
//! overview fades.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::kitty::{self, Compression};
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

/// A thumbnail to upload, at an opacity step, then place.
pub(super) struct Upload {
    pane: PaneId,
    step: u8,
    compression: Compression,
}

impl Upload {
    /// Compresses `image` as this upload wants it: faded to its step first,
    /// unless that's fully opaque.
    fn compress(&self, image: &thumbnail::Image) -> kitty::Compressed {
        if self.step == OPACITY_STEPS {
            return kitty::compress(image, self.compression);
        }
        let opacity = f32::from(self.step) / f32::from(OPACITY_STEPS);
        kitty::compress(&image.with_opacity(opacity), self.compression)
    }
}

impl Thumbnail {
    /// Shows this as pane `pane`'s thumbnail: places it at `size` cells if
    /// it isn't already, and returns the upload it needs if it isn't there
    /// at `opacity` (rounded to a few steps). The upload places it too.
    fn show(
        &mut self,
        escapes: &mut Vec<u8>,
        pane: PaneId,
        size: (u16, u16),
        opacity: f32,
        compression: Compression,
    ) -> Option<Upload> {
        let step = opacity_step(opacity);
        if self.opacity != Some(step) {
            // Only a thumbnail parked with the overview closed has no image,
            // and it's drawn again before it needs one.
            self.image.as_ref()?;
            self.opacity = Some(step);
            self.size = size;
            // A faded image is soon replaced by a less faded one.
            let compression = if step == OPACITY_STEPS {
                compression
            } else {
                Compression::Fast
            };
            return Some(Upload { pane, step, compression });
        }
        if self.size != size {
            kitty::place(escapes, image_id(pane), size.0, size.1);
            self.size = size;
        }
        None
    }
}

/// Compresses each job's image, on as many threads as there are cores to
/// spare, up to one per job. One job runs here, with no thread to start.
fn compress_all(
    jobs: &[(&Upload, &thumbnail::Image, (u16, u16))],
) -> Vec<kitty::Compressed> {
    let compress = |chunk: &[(&Upload, &thumbnail::Image, (u16, u16))]| {
        (chunk.iter())
            .map(|(upload, image, _)| upload.compress(image))
            .collect::<Vec<_>>()
    };
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let threads = cores.min(jobs.len());
    if threads <= 1 {
        return compress(jobs);
    }
    std::thread::scope(|scope| {
        let running: Vec<_> = (jobs.chunks(jobs.len().div_ceil(threads)))
            .map(|chunk| scope.spawn(move || compress(chunk)))
            .collect();
        (running.into_iter())
            .flat_map(|thread| thread.join().expect("compressing can't panic"))
            .collect()
    })
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
    /// pane changed (at most every [`THUMBNAIL_INTERVAL`]), and sizes its
    /// placement to `size` cells. Returns the upload it needs at `opacity`
    /// (0 to 1, in a few steps, for fading it in), for [`Self::upload`].
    pub(super) fn refresh_thumbnail(
        &mut self,
        panes: &HashMap<PaneId, Pane>,
        id: PaneId,
        size: (u16, u16),
        opacity: f32,
        now: Instant,
    ) -> Option<Upload> {
        let pane = panes.get(&id)?;
        let image_id = image_id(id);
        let generation = pane.emulator().generation();
        let stale = self.thumbnails.get(&id).is_none_or(|t| {
            (t.generation != generation && now >= t.uploaded + THUMBNAIL_INTERVAL)
                // Parked, and wanted at another opacity than it was left at.
                || (t.image.is_none() && t.opacity != Some(opacity_step(opacity)))
        });
        if stale {
            let image = thumbnail::rasterize(
                pane.emulator().term(),
                &self.palette,
                self.thumbnail_cell,
            );
            let Some(image) = image else {
                // Too big for a thumbnail; the overview shows its text.
                if self.thumbnails.remove(&id).is_some() {
                    kitty::delete(&mut self.escapes, image_id);
                }
                return None;
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
        // Mid-fade, the overview can't wait for small images; once it's
        // still, they're worth the wait.
        let compression = if self.transition.is_some() {
            Compression::Fast
        } else {
            Compression::Small
        };
        let thumb = self.thumbnails.get_mut(&id).expect("inserted if missing");
        thumb.show(&mut self.escapes, id, size, opacity, compression)
    }

    /// The uploads that show the thumbnails already shown at `opacity`, as
    /// the overview fades out.
    pub(super) fn fade_thumbnails(&mut self, opacity: f32) -> Vec<Upload> {
        let escapes = &mut self.escapes;
        (self.thumbnails.iter_mut())
            .filter_map(|(id, thumb)| {
                let size = thumb.size;
                thumb.show(escapes, *id, size, opacity, Compression::Fast)
            })
            .collect()
    }

    /// Sends `uploads`, each followed by its placement. Compressing is most
    /// of the work, and each image's is its own, so a frame's are spread
    /// over the cores: opening the overview uploads every thumbnail at once.
    pub(super) fn upload(&mut self, uploads: Vec<Upload>) {
        let thumbnails = &self.thumbnails;
        let jobs: Vec<_> = (uploads.iter())
            .filter_map(|upload| {
                let thumb = thumbnails.get(&upload.pane)?;
                Some((upload, thumb.image.as_ref()?, thumb.size))
            })
            .collect();
        let compressed = compress_all(&jobs);
        for ((upload, _, size), image) in jobs.iter().zip(&compressed) {
            let id = image_id(upload.pane);
            kitty::transmit_compressed(&mut self.escapes, id, image);
            kitty::place(&mut self.escapes, id, size.0, size.1);
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
