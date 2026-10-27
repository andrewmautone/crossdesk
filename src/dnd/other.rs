//! Other systems: files can still arrive, they just go to Downloads.

use crate::state::Shared;
use std::path::PathBuf;
use std::sync::Arc;

pub fn start(_shared: Arc<Shared>) {}

pub fn local_drag_over(_crossed: bool) {}

pub fn begin_drag(paths: Vec<PathBuf>) {
    super::drag_finished(paths, false);
}
