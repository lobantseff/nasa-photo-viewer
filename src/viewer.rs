//! Cursor-anchored zoom and drag-pan for a single image.

use egui::{Rect, Vec2};

/// Viewport transform for the detail view.
///
/// Zoom is stored as a scale factor relative to "fit to window"; panning is an
/// offset in screen pixels from the centred position.
#[derive(Debug, Clone, Copy)]
pub struct ZoomPan {
    pub scale: f32,
    pub offset: Vec2,
    /// Set when the view should re-fit on the next frame, e.g. after the
    /// selected image changes.
    pub needs_fit: bool,
}

impl Default for ZoomPan {
    fn default() -> Self {
        Self {
            scale: 1.0,
            offset: Vec2::ZERO,
            needs_fit: true,
        }
    }
}

pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 40.0;

impl ZoomPan {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Scale that fits `image` inside `viewport` without cropping.
    pub fn fit_scale(image: Vec2, viewport: Vec2) -> f32 {
        if image.x <= 0.0 || image.y <= 0.0 {
            return 1.0;
        }
        (viewport.x / image.x)
            .min(viewport.y / image.y)
            .max(MIN_SCALE)
    }

    /// Zoom by `factor`, keeping the image point under `anchor` stationary.
    ///
    /// Anchoring on the cursor is what makes scroll-zoom feel direct; zooming
    /// about the centre instead makes the target drift away as you magnify.
    pub fn zoom_at(&mut self, factor: f32, anchor: egui::Pos2, viewport: Rect) {
        let old = self.scale;
        let new = (old * factor).clamp(MIN_SCALE, MAX_SCALE);
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

    /// Keep the image from being dragged entirely off screen.
    pub fn clamp_to(&mut self, size: Vec2, viewport: Rect) {
        let scaled = size * self.scale;
        // Always leave a sliver visible so the image can be recovered.
        let margin = viewport.size() * 0.5 + scaled * 0.5 - Vec2::splat(32.0);
        self.offset.x = self.offset.x.clamp(-margin.x.max(0.0), margin.x.max(0.0));
        self.offset.y = self.offset.y.clamp(-margin.y.max(0.0), margin.y.max(0.0));
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
        // A wide image is limited by width.
        assert_eq!(
            ZoomPan::fit_scale(vec2(2000.0, 800.0), vec2(1000.0, 800.0)),
            0.5
        );
        // A tall image is limited by height.
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
    fn zoom_keeps_the_anchored_point_stationary() {
        let vp = viewport();
        let size = vec2(1000.0, 800.0);
        let mut zp = ZoomPan {
            scale: 1.0,
            offset: Vec2::ZERO,
            needs_fit: false,
        };

        // Point in image space under the cursor before zooming.
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
    fn zoom_is_clamped_at_both_ends() {
        let vp = viewport();
        let mut zp = ZoomPan::default();

        for _ in 0..100 {
            zp.zoom_at(2.0, vp.center(), vp);
        }
        assert_eq!(zp.scale, MAX_SCALE);

        for _ in 0..200 {
            zp.zoom_at(0.5, vp.center(), vp);
        }
        assert_eq!(zp.scale, MIN_SCALE);
    }

    #[test]
    fn panning_accumulates_and_reset_restores_defaults() {
        let mut zp = ZoomPan::default();
        zp.pan(vec2(10.0, -5.0));
        zp.pan(vec2(2.0, 1.0));
        assert_eq!(zp.offset, vec2(12.0, -4.0));

        zp.reset();
        assert_eq!(zp.offset, Vec2::ZERO);
        assert_eq!(zp.scale, 1.0);
        assert!(zp.needs_fit);
    }

    #[test]
    fn clamping_keeps_the_image_reachable() {
        let vp = viewport();
        let size = vec2(500.0, 400.0);
        let mut zp = ZoomPan {
            scale: 1.0,
            offset: vec2(100_000.0, 100_000.0),
            needs_fit: false,
        };

        zp.clamp_to(size, vp);

        let rect = zp.image_rect(size, vp);
        assert!(
            rect.intersects(vp),
            "image was allowed to leave the viewport entirely"
        );
    }
}
