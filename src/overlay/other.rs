pub struct Overlay;

impl Overlay {
    /// Liquid Glass exists only on macOS.
    pub fn glass_available() -> bool {
        false
    }

    pub fn present_glass(&mut self, _center: (i32, i32), _logical: f32, _radius: f32, _alpha: f32) {
    }

    pub fn new() -> Option<Overlay> {
        None
    }
    pub fn pixel_scale(_monitor_scale: f32) -> f32 {
        1.0
    }
    pub fn present(&mut self, _rgba: &[u8], _side: usize, _center: (i32, i32), _logical: f32) {}
    pub fn hide(&mut self) {}
    pub fn pump(&self) {}
    pub fn capture(
        _center: (i32, i32),
        _side: usize,
        _logical: f32,
    ) -> Option<crate::effects::Snapshot> {
        None
    }
}
