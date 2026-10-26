pub struct Overlay;

impl Overlay {
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
