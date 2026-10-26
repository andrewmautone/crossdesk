use crate::layout::Layout;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub name: String,
    pub os: String,
    pub key: String,
    #[serde(default)]
    pub last_addr: Option<String>,
}

impl TrustedPeer {
    pub fn key_bytes(&self) -> Option<[u8; 32]> {
        hex::decode(&self.key).ok()?.try_into().ok()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Move to other machines by pushing the cursor against a screen edge.
    pub edge_switch: bool,
    /// Pixels at each end of an edge where switching is ignored (corners stay usable).
    pub corner_px: i32,
    /// How far (pixels) to keep pushing past an edge before the cursor crosses; 0 = at once.
    pub edge_resistance: i32,
    pub clipboard_sync: bool,
    /// Liquid ripple where the cursor leaves and enters a screen.
    pub effects: bool,
    /// Refract the real screen content (needs Screen Recording on macOS).
    pub distort: bool,
    /// When the other side runs a different OS, swap Ctrl with Cmd/Win.
    pub swap_ctrl_cmd: bool,
    /// Lines scrolled per wheel notch when this machine is controlled (macOS).
    pub scroll_lines: f32,
    pub invert_scroll: bool,
    /// Closing the window keeps crossdesk running in the tray / menu bar.
    pub close_to_tray: bool,
    /// Start with only the tray icon, no window.
    pub start_hidden: bool,
    /// Install new builds from GitHub Releases automatically.
    pub auto_update: bool,
    /// GitHub "owner/repo" that publishes the releases.
    pub update_repo: String,
    /// Only needed when the repository is private.
    pub github_token: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            edge_switch: true,
            corner_px: 0,
            edge_resistance: 120,
            clipboard_sync: true,
            effects: true,
            distort: true,
            swap_ctrl_cmd: true,
            scroll_lines: 3.0,
            invert_scroll: false,
            close_to_tray: true,
            start_hidden: false,
            auto_update: true,
            update_repo: "andrewmautone/crossdesk".into(),
            github_token: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub id: String,
    pub name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub trusted: BTreeMap<String, TrustedPeer>,
    #[serde(default)]
    pub layout: Layout,
    #[serde(default)]
    pub settings: Settings,
    /// In-memory only (tests); never written to disk.
    #[serde(skip)]
    pub ephemeral: bool,
}

fn default_port() -> u16 {
    crate::protocol::TCP_PORT
}

pub fn path() -> PathBuf {
    // A second copy for development can run with its own identity.
    if let Some(p) = std::env::var_os("CROSSDESK_CONFIG") {
        return PathBuf::from(p);
    }
    match directories::ProjectDirs::from("com", "crossdesk", "Crossdesk") {
        Some(d) => d.config_dir().join("config.json"),
        None => PathBuf::from("crossdesk-config.json"),
    }
}

impl Config {
    pub fn load() -> Config {
        let p = path();
        if let Ok(text) = std::fs::read_to_string(&p) {
            match serde_json::from_str::<Config>(&text) {
                Ok(c) => return c,
                Err(e) => eprintln!("config {} unreadable ({e}); starting fresh", p.display()),
            }
        }
        let name = gethostname::gethostname()
            .to_string_lossy()
            .trim_end_matches(".local")
            .to_string();
        let c = Config {
            id: crate::crypto::random_id(),
            name,
            port: default_port(),
            trusted: BTreeMap::new(),
            layout: Layout::default(),
            settings: Settings::default(),
            ephemeral: false,
        };
        c.save();
        c
    }

    pub fn save(&self) {
        if self.ephemeral {
            return;
        }
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("json.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            }
            Err(e) => eprintln!("cannot save config: {e}"),
        }
    }
}
