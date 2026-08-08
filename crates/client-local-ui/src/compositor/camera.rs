//! The infinite-canvas camera and the pan easing that follows it.

use super::*;

/// The infinite-canvas camera: windows live in canvas space and map to screen
/// space by panning (scroll) and zooming (Cmd/Ctrl + scroll).
#[derive(Clone, Copy)]
pub(super) struct Camera {
    pub(super) pan: [f32; 2],
    pub(super) zoom: f32,
}

impl Camera {
    pub(super) fn to_screen(self, p: [f32; 2]) -> [f32; 2] {
        [
            self.pan[0] + p[0] * self.zoom,
            self.pan[1] + p[1] * self.zoom,
        ]
    }
    pub(super) fn to_canvas(self, p: [f32; 2]) -> [f32; 2] {
        [
            (p[0] - self.pan[0]) / self.zoom,
            (p[1] - self.pan[1]) / self.zoom,
        ]
    }
    pub(super) fn zoom_at(&mut self, factor: f32, focus: [f32; 2]) {
        let anchor = self.to_canvas(focus);
        self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.pan = [
            focus[0] - anchor[0] * self.zoom,
            focus[1] - anchor[1] * self.zoom,
        ];
    }
}

/// Zoom limits and the fixed presets offered by the corner zoom menu.
pub(super) const ZOOM_MIN: f32 = 0.2;
pub(super) const ZOOM_MAX: f32 = 2.0;
pub(super) const ZOOM_PRESETS: [f32; 4] = [2.0, 1.5, 1.0, 0.5];

/// Ease `current` toward `target`, snapping when within half a pixel.
pub(super) fn ease(current: f32, target: f32) -> f32 {
    let d = target - current;
    if d.abs() < 0.5 {
        target
    } else {
        current + d * PAN_SMOOTH
    }
}
