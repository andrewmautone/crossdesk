//! Text clipboard sharing. Clipboard access can be slow, so it runs on its own
//! thread; callers only queue requests.

use crate::net::Registry;
use crate::protocol::Msg;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

const MAX_TEXT: usize = 1024 * 1024;

enum Cmd {
    SendTo(String),
    Set(String),
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
                // Last text we wrote or sent, so we do not bounce the same text back and forth.
                let mut last: Option<String> = None;
                for cmd in rx {
                    if board.is_none() {
                        board = arboard::Clipboard::new().ok();
                    }
                    let Some(b) = board.as_mut() else { continue };
                    match cmd {
                        Cmd::SendTo(peer) => {
                            if let Ok(text) = b.get_text() {
                                if text.len() <= MAX_TEXT && last.as_ref() != Some(&text) {
                                    last = Some(text.clone());
                                    registry.send(&peer, Msg::Clipboard(text));
                                }
                            }
                        }
                        Cmd::Set(text) => {
                            if text.len() <= MAX_TEXT && b.get_text().ok().as_ref() != Some(&text) {
                                let _ = b.set_text(text.clone());
                            }
                            last = Some(text);
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
}
