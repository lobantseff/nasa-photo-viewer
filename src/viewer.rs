//! Cursor-anchored zoom and drag-pan for a single image.

use egui::{Rect, Vec2};

/// Hard ceiling on magnification, well past pixel-peeping range.
pub const MAX_SCALE: f32 = 40.0;

/// Absolute floor, only reached for degenerate image sizes.
const ABSOLUTE_MIN_SCALE: f32 = 0.01;

/// Viewport transform for the detail view.
///
/// `scale` is true pixel scale: 1.0 draws one image pixel per point.
#[derive(Debug, Clone, Copy)]
pub struct ZoomPan {
    pub scale: f32,
    pub offset: Vec2,
    /// Set when the view should re-fit, e.g. after the selection changes.
    pub needs_fit: bool,
    /// Smallest scale the user can reach, derived from the current image and
    /// viewport by [`ZoomPan::set_bounds`].
    min_scale: f32,
}

impl Default for ZoomPan {
    fn default() -> Self {
        Self {
            scale: 1.0,
            offset: Vec2::ZERO,
            needs_fit: true,
            min_scale: ABSOLUTE_MIN_SCALE,
        }
    }
}

impl ZoomPan {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn min_scale(&self) -> f32 {
        self.min_scale
    }

    /// Scale at which `image` exactly fits inside `viewport`.
    pub fn fit_scale(image: Vec2, viewport: Vec2) -> f32 {
        if image.x <= 0.0 || image.y <= 0.0 || !image.is_finite() {
            return 1.0;
        }
        (viewport.x / image.x)
            .min(viewport.y / image.y)
            .max(ABSOLUTE_MIN_SCALE)
    }

    /// Smallest *useful* scale for this image and viewport.
    ///
    /// Zooming out further than "the whole image is visible" only shrinks the
    /// picture into empty space, so that is the floor. An image smaller than
    /// the viewport stops at 1:1 instead, since blowing it up to fill the
    /// window would only magnify its pixels.
    pub fn min_scale_for(image: Vec2, viewport: Vec2) -> f32 {
        Self::fit_scale(image, viewport).min(1.0)
    }

    /// Recompute the zoom floor and pull the current scale up to it.
    ///
    /// Called every frame because the floor moves when the window resizes or
    /// a higher-resolution rendition replaces a preview.
    pub fn set_bounds(&mut self, image: Vec2, viewport: Vec2) {
        self.min_scale = Self::min_scale_for(image, viewport);
        if self.scale < self.min_scale {
            self.scale = self.min_scale;
        }
    }

    /// Fit the image to the viewport, or show it 1:1 if it is smaller.
    pub fn fit(&mut self, image: Vec2, viewport: Vec2) {
        self.set_bounds(image, viewport);
        self.scale = self.min_scale;
        self.offset = Vec2::ZERO;
        self.needs_fit = false;
    }

    /// Zoom by `factor`, keeping the image point under `anchor` stationary.
    ///
    /// Anchoring on the cursor is what makes scroll-zoom feel direct; zooming
    /// about the centre instead makes the target drift away as you magnify.
    pub fn zoom_at(&mut self, factor: f32, anchor: egui::Pos2, viewport: Rect) {
        let old = self.scale;
        let new = (old * factor).clamp(self.min_scale, MAX_SCALE);
        if (new - old).abs() < f32::EPSILON {
            return;
        }

        // Offset of the anchor from the image centre, in screen pixels.
        let centre = viewport.center() + self.offset;
        let to_anchor = anchor - centre;
        let ratio = new / old;

        self.offset += to_anchor - to_anchor * ratio;
        self.scale = new;
    }

    pub fn pan(&mut self, delta: Vec2) {
        self.offset += delta;
    }

    /// Where to draw an image of `size` (in image pixels) within `viewport`.
    pub fn image_rect(&self, size: Vec2, viewport: Rect) -> Rect {
        let scaled = size * self.scale;
        Rect::from_center_size(viewport.center() + self.offset, scaled)
    }

    /// Keep the image the same size on screen when its texture is replaced by
    /// one of a different resolution.
    ///
    /// `scale` is relative to the source pixels, so swapping a 1200px preview
    /// for the full-resolution original would otherwise make the picture jump
    /// the moment the upgrade arrives.
    pub fn preserve_apparent_size(&mut self, old: Vec2, new: Vec2) {
        if old.x > 0.0 && new.x > 0.0 && old.is_finite() && new.is_finite() {
            self.scale *= old.x / new.x;
        }
    }

    /// Whether any part of the image is currently off-screen.
    ///
    /// Drives both the grab cursor and whether drag gestures do anything: a
    /// fully visible image has nowhere to pan to.
    pub fn is_pannable(&self, size: Vec2, viewport: Vec2) -> bool {
        let scaled = size * self.scale;
        scaled.x > viewport.x + 0.5 || scaled.y > viewport.y + 0.5
    }

    /// Constrain panning so the image cannot be dragged away from the view.
    ///
    /// An axis that fits entirely is centred: sliding a fully visible image
    /// around the void serves no purpose. An axis larger than the viewport is
    /// limited to its own edges, so no empty gap can open beside it.
    pub fn clamp_to(&mut self, size: Vec2, viewport: Rect) {
        let scaled = size * self.scale;
        let limit = |scaled: f32, avail: f32, offset: f32| -> f32 {
            if scaled <= avail {
                0.0
            } else {
                let max = (scaled - avail) * 0.5;
                offset.clamp(-max, max)
            }
        };

        self.offset.x = limit(scaled.x, viewport.width(), self.offset.x);
        self.offset.y = limit(scaled.y, viewport.height(), self.offset.y);
    }
}

/// A pointer or trackpad gesture over the image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// Pinch, or ctrl+scroll, which egui reports uniformly as a zoom factor.
    Zoom(f32),
    /// Two-finger drag, or a plain wheel, which scrolls rather than zooms.
    Pan(Vec2),
    None,
}

/// How much one wheel line magnifies. `exp` keeps successive ticks
/// multiplicative, so zooming in then out returns to the original scale.
const WHEEL_ZOOM_RATE: f32 = 0.15;

/// Classify this frame's pointer input.
///
/// A mouse wheel and a trackpad swipe both arrive as scroll events, and are
/// told apart by their unit: wheels report discrete `Line`s, trackpads report
/// continuous `Point`s. That distinction is what lets a wheel zoom while a
/// two-finger swipe pans.
///
/// Zoom wins over pan, because a trackpad pinch frequently carries a small
/// incidental scroll that would otherwise make the image creep while
/// magnifying.
pub fn gesture_from(zoom_delta: f32, wheel_lines: f32, point_scroll: Vec2) -> Gesture {
    const ZOOM_EPS: f32 = 1e-4;

    if (zoom_delta - 1.0).abs() > ZOOM_EPS && zoom_delta > 0.0 {
        Gesture::Zoom(zoom_delta)
    } else if wheel_lines != 0.0 {
        Gesture::Zoom((wheel_lines * WHEEL_ZOOM_RATE).exp())
    } else if point_scroll != Vec2::ZERO {
        Gesture::Pan(point_scroll)
    } else {
        Gesture::None
    }
}

/// Whether the preview is being magnified enough to warrant the
/// full-resolution original.
///
/// The trigger is the preview being drawn larger than its own pixels, which is
/// exactly when it starts to look soft. Merely opening an image never trips
/// it, so the multi-megabyte original is fetched only once it would actually
/// show more detail.
pub fn should_upgrade_to_full_res(scale: f32, min_scale: f32) -> bool {
    const EPS: f32 = 1e-4;
    scale > min_scale + EPS && scale > 1.0 + EPS
}

/// Cursor to show over the image.
///
/// Returns `None` when the image fits entirely, since there is nothing to drag
/// and a grab cursor would promise interaction that does nothing.
pub fn cursor_for(pannable: bool, dragging: bool) -> Option<egui::CursorIcon> {
    match (pannable, dragging) {
        (true, true) => Some(egui::CursorIcon::Grabbing),
        (true, false) => Some(egui::CursorIcon::Grab),
        (false, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, pos2, vec2};

    fn viewport() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))
    }

    #[test]
    fn fit_scale_uses_the_limiting_axis() {
        assert_eq!(
            ZoomPan::fit_scale(vec2(2000.0, 800.0), vec2(1000.0, 800.0)),
            0.5
        );
        assert_eq!(
            ZoomPan::fit_scale(vec2(1000.0, 1600.0), vec2(1000.0, 800.0)),
            0.5
        );
    }

    #[test]
    fn fit_scale_survives_a_degenerate_image() {
        assert_eq!(ZoomPan::fit_scale(vec2(0.0, 0.0), vec2(100.0, 100.0)), 1.0);
    }

    #[test]
    fn a_large_image_cannot_be_zoomed_out_past_fitting() {
        let vp = viewport();
        let image = vec2(2000.0, 1600.0);
        let mut zp = ZoomPan::default();
        zp.fit(image, vp.size());

        // Fit is the floor, so the image always fills one axis of the view.
        assert_eq!(zp.scale, 0.5);

        for _ in 0..50 {
            zp.zoom_at(0.5, vp.center(), vp);
        }
        assert_eq!(
            zp.scale, 0.5,
            "zooming out past fit leaves the image stranded in empty space"
        );
    }

    #[test]
    fn a_small_image_stops_at_one_to_one() {
        let vp = viewport();
        // Smaller than the viewport: fitting would upscale it 3x.
        let image = vec2(320.0, 240.0);
        let mut zp = ZoomPan::default();
        zp.fit(image, vp.size());

        assert_eq!(zp.scale, 1.0, "a small image should open at native size");

        for _ in 0..50 {
            zp.zoom_at(0.5, vp.center(), vp);
        }
        assert_eq!(zp.scale, 1.0);
    }

    #[test]
    fn zoom_in_is_still_capped() {
        let vp = viewport();
        let mut zp = ZoomPan::default();
        zp.fit(vec2(2000.0, 1600.0), vp.size());

        for _ in 0..100 {
            zp.zoom_at(2.0, vp.center(), vp);
        }
        assert_eq!(zp.scale, MAX_SCALE);
    }

    #[test]
    fn resizing_the_window_raises_the_floor() {
        let image = vec2(2000.0, 1600.0);
        let mut zp = ZoomPan::default();
        zp.fit(image, vec2(1000.0, 800.0));
        assert_eq!(zp.scale, 0.5);

        // Shrinking the window lowers the fit scale; the current scale stays.
        zp.set_bounds(image, vec2(500.0, 400.0));
        assert_eq!(zp.min_scale(), 0.25);
        assert_eq!(zp.scale, 0.5);

        // Growing it raises the floor, which must pull the scale up with it.
        zp.set_bounds(image, vec2(4000.0, 3200.0));
        assert_eq!(zp.min_scale(), 1.0);
        assert_eq!(zp.scale, 1.0);
    }

    #[test]
    fn zoom_keeps_the_anchored_point_stationary() {
        let vp = viewport();
        let size = vec2(1000.0, 800.0);
        let mut zp = ZoomPan::default();
        zp.fit(size, vp.size());

        let anchor = pos2(700.0, 300.0);
        let before = (anchor - zp.image_rect(size, vp).min) / zp.scale;

        zp.zoom_at(2.0, anchor, vp);

        let after = (anchor - zp.image_rect(size, vp).min) / zp.scale;
        assert!(
            (before - after).length() < 0.01,
            "anchored image point drifted: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn a_fitted_image_stays_centred() {
        let vp = viewport();
        let image = vec2(2000.0, 1600.0);
        let mut zp = ZoomPan::default();
        zp.fit(image, vp.size());

        zp.pan(vec2(400.0, -300.0));
        zp.clamp_to(image, vp);

        // At fit the whole image is visible, so dragging must not move it.
        assert_eq!(zp.offset, Vec2::ZERO);
    }

    #[test]
    fn a_zoomed_image_cannot_be_dragged_past_its_own_edge() {
        let vp = viewport();
        let image = vec2(2000.0, 1600.0);
        let mut zp = ZoomPan::default();
        zp.fit(image, vp.size());
        zp.zoom_at(4.0, vp.center(), vp);

        zp.pan(vec2(100_000.0, 100_000.0));
        zp.clamp_to(image, vp);

        // No empty gap may open between the image edge and the viewport edge.
        let rect = zp.image_rect(image, vp);
        assert!(
            rect.min.x <= vp.min.x + 0.01 && rect.max.x >= vp.max.x - 0.01,
            "horizontal gap opened: {rect:?} vs {vp:?}"
        );
        assert!(
            rect.min.y <= vp.min.y + 0.01 && rect.max.y >= vp.max.y - 0.01,
            "vertical gap opened: {rect:?} vs {vp:?}"
        );
    }

    #[test]
    fn pannability_tracks_whether_the_image_overflows_the_view() {
        let vp = vec2(1000.0, 800.0);
        let image = vec2(2000.0, 1600.0);
        let mut zp = ZoomPan::default();

        zp.fit(image, vp);
        assert!(
            !zp.is_pannable(image, vp),
            "a fitted image has nowhere to go"
        );

        zp.zoom_at(2.0, Pos2::ZERO, Rect::from_min_size(Pos2::ZERO, vp));
        assert!(zp.is_pannable(image, vp), "a zoomed image must be pannable");
    }

    #[test]
    fn a_pinch_is_read_as_zoom_and_a_trackpad_swipe_as_pan() {
        assert_eq!(gesture_from(1.5, 0.0, Vec2::ZERO), Gesture::Zoom(1.5));
        assert_eq!(gesture_from(0.5, 0.0, Vec2::ZERO), Gesture::Zoom(0.5));
        assert_eq!(
            gesture_from(1.0, 0.0, vec2(3.0, -7.0)),
            Gesture::Pan(vec2(3.0, -7.0))
        );
        assert_eq!(gesture_from(1.0, 0.0, Vec2::ZERO), Gesture::None);
    }

    #[test]
    fn a_mouse_wheel_zooms_rather_than_pans() {
        // Wheels report whole lines; trackpads report points. Only the
        // line-based ones should magnify.
        let Gesture::Zoom(inward) = gesture_from(1.0, 1.0, Vec2::ZERO) else {
            panic!("a wheel tick should zoom");
        };
        assert!(inward > 1.0, "scrolling up should zoom in, got {inward}");

        let Gesture::Zoom(outward) = gesture_from(1.0, -1.0, Vec2::ZERO) else {
            panic!("a wheel tick should zoom");
        };
        assert!(
            outward < 1.0,
            "scrolling down should zoom out, got {outward}"
        );

        // Opposite ticks must cancel exactly, or repeated in-and-out drifts.
        assert!((inward * outward - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_wheel_tick_outranks_a_simultaneous_point_scroll() {
        assert!(matches!(
            gesture_from(1.0, 1.0, vec2(0.0, 5.0)),
            Gesture::Zoom(_)
        ));
    }

    #[test]
    fn a_pinch_carrying_incidental_scroll_still_zooms() {
        assert_eq!(gesture_from(1.2, 0.0, vec2(2.0, 2.0)), Gesture::Zoom(1.2));
    }

    #[test]
    fn a_degenerate_zoom_factor_is_ignored() {
        assert_eq!(gesture_from(0.0, 0.0, Vec2::ZERO), Gesture::None);
    }

    #[test]
    fn full_res_is_requested_only_once_the_preview_is_magnified() {
        // A large image opens fitted and downscaled: nothing to gain yet.
        assert!(!should_upgrade_to_full_res(0.5, 0.5));
        // Zoomed, but the preview is still being shrunk.
        assert!(!should_upgrade_to_full_res(0.9, 0.5));
        // Now drawn larger than its own pixels, so it looks soft.
        assert!(should_upgrade_to_full_res(1.5, 0.5));
    }

    #[test]
    fn opening_a_small_image_does_not_trigger_an_upgrade() {
        // A small image opens at 1:1, which must not count as magnifying.
        assert!(!should_upgrade_to_full_res(1.0, 1.0));
        assert!(should_upgrade_to_full_res(1.4, 1.0));
    }

    #[test]
    fn upgrading_the_texture_keeps_the_image_the_same_size_on_screen() {
        let vp = viewport();
        let preview = vec2(1200.0, 900.0);
        let full = vec2(2400.0, 1800.0);

        let mut zp = ZoomPan::default();
        zp.fit(preview, vp.size());
        zp.zoom_at(3.0, vp.center(), vp);
        let before = zp.image_rect(preview, vp);

        zp.preserve_apparent_size(preview, full);
        let after = zp.image_rect(full, vp);

        assert!(
            (before.size() - after.size()).length() < 0.01,
            "image jumped on upgrade: {:?} -> {:?}",
            before.size(),
            after.size()
        );
    }

    #[test]
    fn preserving_size_ignores_degenerate_dimensions() {
        let mut zp = ZoomPan {
            scale: 2.0,
            ..Default::default()
        };
        zp.preserve_apparent_size(Vec2::ZERO, vec2(100.0, 100.0));
        assert_eq!(zp.scale, 2.0);
    }

    #[test]
    fn the_grab_cursor_only_appears_when_there_is_something_to_drag() {
        assert_eq!(cursor_for(true, false), Some(egui::CursorIcon::Grab));
        assert_eq!(cursor_for(true, true), Some(egui::CursorIcon::Grabbing));
        assert_eq!(cursor_for(false, false), None);
        assert_eq!(cursor_for(false, true), None);
    }

    #[test]
    fn panning_accumulates_and_reset_restores_defaults() {
        let mut zp = ZoomPan::default();
        zp.pan(vec2(10.0, -5.0));
        zp.pan(vec2(2.0, 1.0));
        assert_eq!(zp.offset, vec2(12.0, -4.0));

        zp.reset();
        assert_eq!(zp.offset, Vec2::ZERO);
        assert!(zp.needs_fit);
    }
}
