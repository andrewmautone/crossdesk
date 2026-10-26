//! Fallback for platforms without capture/injection support yet.

use super::Backend;
use crate::layout::Monitor;
use crate::protocol::Button;
use crate::state::Shared;
use std::sync::Arc;

pub fn init_process() {}

pub fn shutdown() {}

pub fn start_capture(shared: Arc<Shared>) {
    *shared.capture_error.lock() = Some("input capture is not supported on this OS yet".into());
}

pub fn warp(_x: i32, _y: i32) {}

#[derive(Default)]
pub struct Platform;

impl Backend for Platform {
    fn monitors() -> Vec<Monitor> {
        vec![Monitor { x: 0, y: 0, w: 1920, h: 1080, primary: true, name: "Display".into() }]
    }
    fn move_abs(&mut self, _x: i32, _y: i32) {}
    fn button(&mut self, _button: Button, _down: bool) {}
    fn wheel(&mut self, _dx: i32, _dy: i32) {}
    fn key(&mut self, _hid: u16, _down: bool) {}
}
