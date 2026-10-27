//! Clipboard sharing: text, images and files. Clipboard access can be slow, so
//! it runs on its own thread; callers only queue requests.

use crate::net::Registry;
use crate::protocol::{Msg, TransferKind};
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::SystemTime;

const MAX_TEXT: usize = 1024 * 1024;

enum Cmd {
    SendTo(String),
    Set(String),
    SetImagePng(PathBuf),
    SetFiles(Vec<PathBuf>),
}

/// What we last wrote or sent, so the same content does not bounce back and forth.
#[derive(PartialEq)]
enum Last {
    Text(String),
    Image(u64),
    Files(u64),
}

#[derive(Clone)]
pub struct Clipboard {
    tx: Sender<Cmd>,
}

impl Clipboard {
    pub fn start(registry: Arc<Registry>) -> Clipboard {
        let (tx, rx) = channel::<Cmd>();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut board = arboard::Clipboard::new().ok();
                let mut last: Option<Last> = None;
                for cmd in rx {
                    if board.is_none() {
                        board = arboard::Clipboard::new().ok();
                    }
                    let Some(b) = board.as_mut() else { continue };
                    match cmd {
                        Cmd::SendTo(peer) => send(b, &registry, &peer, &mut last),
                        Cmd::Set(text) => {
                            if text.len() <= MAX_TEXT && b.get_text().ok().as_ref() != Some(&text) {
                                let _ = b.set_text(text.clone());
                            }
                            last = Some(Last::Text(text));
                        }
                        Cmd::SetImagePng(path) => {
                            if let Some((w, h, rgba)) = decode_png(&path) {
                                let image = arboard::ImageData {
                                    width: w,
                                    height: h,
                                    bytes: Cow::Owned(rgba),
                                };
                                if b.set_image(image).is_ok() {
                                    // Hash what the clipboard hands back: it may convert the pixels.
                                    last = b.get_image().ok().map(|i| Last::Image(image_hash(&i)));
                                }
                            }
                            let _ = std::fs::remove_file(&path);
                        }
                        Cmd::SetFiles(paths) => {
                            if b.set().file_list(&paths).is_ok() {
                                last = Some(Last::Files(files_hash(&paths)));
                            }
                        }
                    }
                }
            })
            .expect("spawn clipboard thread");
        Clipboard { tx }
    }

    pub fn send_to(&self, peer: &str) {
        let _ = self.tx.send(Cmd::SendTo(peer.to_string()));
    }

    pub fn set(&self, text: String) {
        let _ = self.tx.send(Cmd::Set(text));
    }

    /// Put a received PNG on the clipboard (the file is removed afterwards).
    pub fn set_image_png(&self, path: PathBuf) {
        let _ = self.tx.send(Cmd::SetImagePng(path));
    }

    /// Put received files on the clipboard, ready to paste.
    pub fn set_files(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(Cmd::SetFiles(paths));
    }
}

/// Send what is on the clipboard, if it changed: copied files first (Finder
/// also puts their names as text), then text (apps that copy text often add a
/// picture of it), then an image.
fn send(b: &mut arboard::Clipboard, registry: &Registry, peer: &str, last: &mut Option<Last>) {
    let files = crate::transfer::supports(peer);
    if files {
        if let Ok(paths) = b.get().file_list() {
            if !paths.is_empty() {
                let key = Last::Files(files_hash(&paths));
                if last.as_ref() != Some(&key) {
                    *last = Some(key);
                    let id = crate::transfer::new_id();
                    crate::transfer::send_paths(peer, id, TransferKind::ClipFiles, paths);
                }
                return;
            }
        }
    }
    if let Ok(text) = b.get_text() {
        if !text.is_empty() {
            if text.len() <= MAX_TEXT && last.as_ref() != Some(&Last::Text(text.clone())) {
                registry.send(peer, Msg::Clipboard(text.clone()));
                *last = Some(Last::Text(text));
            }
            return;
        }
    }
    if files {
        if let Ok(image) = b.get_image() {
            let key = Last::Image(image_hash(&image));
            if last.as_ref() != Some(&key) {
                *last = Some(key);
                if let Some(png) = encode_png(&image) {
                    crate::transfer::send_bytes(peer, TransferKind::ClipImage, "image.png", png);
                }
            }
        }
    }
}

fn image_hash(i: &arboard::ImageData) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (i.width, i.height).hash(&mut h);
    i.bytes.hash(&mut h);
    h.finish()
}

/// Same files, unchanged since: same hash.
fn files_hash(paths: &[PathBuf]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in paths {
        p.hash(&mut h);
        if let Ok(m) = std::fs::metadata(p) {
            m.len().hash(&mut h);
            m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).hash(&mut h);
        }
    }
    h.finish()
}

fn encode_png(i: &arboard::ImageData) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, i.width as u32, i.height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header().ok()?.write_image_data(&i.bytes).ok()?;
    Some(out)
}

fn decode_png(path: &std::path::Path) -> Option<(usize, usize, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::fs::File::open(path).ok()?);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let bytes: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8 * 9).collect();
        let image = arboard::ImageData {
            width: 3,
            height: 2,
            bytes: Cow::Owned(bytes.clone()),
        };
        let png = encode_png(&image).unwrap();
        let path = std::env::temp_dir().join(format!("crossdesk-{}.png", rand::random::<u32>()));
        std::fs::write(&path, png).unwrap();
        let (w, h, rgba) = decode_png(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!((w, h, rgba), (3, 2, bytes));
    }
}
