//! Self-update from GitHub Releases.
//!
//! CI publishes every push to main as a release tagged `build-<N>` with one
//! asset per platform plus a `.sha256` file for each. When auto update is on,
//! this checks now and then, waits until nobody is using the mouse across
//! machines, swaps the installed app for the new one and restarts it.

use crate::state::Shared;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

/// Build number baked in by CI. Local builds are 0 and never update on their own.
pub fn build() -> u64 {
    option_env!("CROSSDESK_BUILD").and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn asset_name() -> Option<&'static str> {
    if cfg!(windows) {
        Some("crossdesk-windows-x64.exe")
    } else if cfg!(target_os = "macos") {
        Some("Crossdesk-macos.zip")
    } else {
        None
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    url: String,
    browser_download_url: String,
}

#[derive(Clone)]
pub struct Updater {
    tx: Sender<bool>,
}

impl Updater {
    pub fn start(shared: Arc<Shared>) -> Updater {
        let (tx, rx) = channel::<bool>();
        std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                // The previous copy may still be exiting right after an update.
                std::thread::sleep(Duration::from_secs(3));
                cleanup_previous();
                // Give the network a moment before the first automatic check.
                let mut wait = Duration::from_secs(17);
                loop {
                    let manual = match rx.recv_timeout(wait) {
                        Ok(manual) => manual,
                        Err(RecvTimeoutError::Timeout) => false,
                        Err(RecvTimeoutError::Disconnected) => return,
                    };
                    wait = CHECK_EVERY;
                    let auto = shared.config.lock().settings.auto_update;
                    if !manual && (!auto || build() == 0) {
                        continue;
                    }
                    if let Err(e) = check_and_install(&shared) {
                        shared.set_update_status(format!("Update failed: {e}"));
                        shared.log(format!("update failed: {e}"));
                    }
                }
            })
            .expect("spawn updater thread");
        Updater { tx }
    }

    /// Check right away (the "Check now" button).
    pub fn check_now(&self) {
        let _ = self.tx.send(true);
    }

    /// Another machine runs a newer build: look for it now instead of waiting.
    pub fn peer_is_newer(&self) {
        let _ = self.tx.send(false);
    }
}

fn agent(redirects: u32) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new();
    // The OS TLS stack (SChannel / Security.framework): no bundled crypto C code.
    if let Ok(tls) = native_tls::TlsConnector::new() {
        builder = builder.tls_connector(Arc::new(tls));
    }
    builder
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .redirects(redirects)
        .user_agent(concat!("crossdesk/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn api_get(url: &str, token: &str, accept: &str) -> Result<ureq::Response, String> {
    let mut req = agent(0).get(url).set("Accept", accept);
    if !token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) if (300..400).contains(&code) => r,
        Err(ureq::Error::Status(404, _)) => {
            return Err("no release found (private repo? add a token in Settings)".into())
        }
        Err(ureq::Error::Status(code, _)) => return Err(format!("GitHub answered {code}")),
        Err(e) => return Err(e.to_string()),
    };
    // Asset downloads redirect to a signed storage URL that must not get our token.
    if (300..400).contains(&resp.status()) {
        let location = resp.header("Location").ok_or("redirect without location")?.to_string();
        return agent(5).get(&location).call().map_err(|e| e.to_string());
    }
    Ok(resp)
}

fn download(asset: &Asset, token: &str) -> Result<Vec<u8>, String> {
    let resp = if token.is_empty() {
        agent(5).get(&asset.browser_download_url).call().map_err(|e| e.to_string())?
    } else {
        api_get(&asset.url, token, "application/octet-stream")?
    };
    let mut bytes = Vec::new();
    resp.into_reader().take(MAX_DOWNLOAD).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn check_and_install(shared: &Arc<Shared>) -> Result<(), String> {
    let (repo, token) = {
        let c = shared.config.lock();
        (c.settings.update_repo.trim().to_string(), c.settings.github_token.trim().to_string())
    };
    if repo.is_empty() {
        return Err("no update repository set".into());
    }
    let name = asset_name().ok_or("updates are not supported on this OS")?;
    shared.set_update_status("Checking for updates…".into());
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let release: Release =
        api_get(&url, &token, "application/vnd.github+json")?.into_json().map_err(|e| e.to_string())?;
    let latest: u64 = release
        .tag_name
        .strip_prefix("build-")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("unexpected release tag {}", release.tag_name))?;
    if latest <= build() {
        shared.set_update_status(format!("Up to date (build {})", build()));
        return Ok(());
    }
    let asset = release.assets.iter().find(|a| a.name == name).ok_or_else(|| format!("release has no {name}"))?;
    let checksum = release.assets.iter().find(|a| a.name == format!("{name}.sha256"));

    // Never swap the app while the mouse is on, or coming from, another machine.
    loop {
        let busy = shared.engine.lock().active_target().is_some() || shared.controlled_by.lock().is_some();
        if !busy {
            break;
        }
        shared.set_update_status(format!("Build {latest} ready; waiting until the mouse is back home"));
        std::thread::sleep(Duration::from_secs(3));
    }

    shared.set_update_status(format!("Downloading build {latest}…"));
    let bytes = download(asset, &token)?;
    if let Some(c) = checksum {
        let text = String::from_utf8(download(c, &token)?).map_err(|_| "bad checksum file")?;
        let expected = text.split_whitespace().next().unwrap_or_default().to_lowercase();
        let actual = hex::encode(Sha256::digest(&bytes));
        if expected != actual {
            return Err("downloaded file is corrupted (checksum mismatch)".into());
        }
    }
    shared.set_update_status(format!("Installing build {latest}…"));
    shared.log(format!("updating from build {} to {latest}", build()));
    let relaunch = install(&bytes)?;
    restart(relaunch);
}

fn restart(target: Relaunch) -> ! {
    crate::input::shutdown();
    let result = match &target {
        Relaunch::Exe(p) => std::process::Command::new(p).spawn().map(|_| ()),
        Relaunch::Bundle(p) => std::process::Command::new("open").arg("-n").arg(p).spawn().map(|_| ()),
    };
    if let Err(e) = result {
        eprintln!("could not restart after update: {e}");
    }
    std::process::exit(0);
}

enum Relaunch {
    Exe(PathBuf),
    #[allow(dead_code)]
    Bundle(PathBuf),
}

fn current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

/// Remove what the previous update left behind (Windows cannot delete a running exe).
fn cleanup_previous() {
    if let Ok(exe) = current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
}

#[cfg(windows)]
fn install(bytes: &[u8]) -> Result<Relaunch, String> {
    let exe = current_exe()?;
    let new = exe.with_extension("new");
    let old = old_path(&exe);
    std::fs::write(&new, bytes).map_err(|e| format!("cannot write {}: {e}", new.display()))?;
    let _ = std::fs::remove_file(&old);
    // A running exe can be renamed but not overwritten.
    std::fs::rename(&exe, &old).map_err(|e| format!("cannot move the running app aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("cannot put the new app in place: {e}"));
    }
    Ok(Relaunch::Exe(exe))
}

#[cfg(target_os = "macos")]
fn install(bytes: &[u8]) -> Result<Relaunch, String> {
    use std::os::unix::fs::PermissionsExt;
    let exe = current_exe()?;
    // .../Crossdesk.app/Contents/MacOS/crossdesk
    let bundle = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|b| b.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf);
    let base = bundle.as_ref().and_then(|b| b.parent()).or(exe.parent()).ok_or("no install folder")?.to_path_buf();
    let work = base.join(format!(".crossdesk-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot write to {}: {e}", base.display()))?;
    let zip = work.join("update.zip");
    std::fs::write(&zip, bytes).map_err(|e| e.to_string())?;
    let ok = std::process::Command::new("ditto")
        .args(["-x", "-k"])
        .arg(&zip)
        .arg(&work)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    let new_app = work.join("Crossdesk.app");
    if !ok || !new_app.exists() {
        let _ = std::fs::remove_dir_all(&work);
        return Err("could not unpack the update".into());
    }
    let result = match &bundle {
        Some(bundle) => {
            let old = work.join("previous.app");
            std::fs::rename(bundle, &old).map_err(|e| format!("cannot move the running app aside: {e}"))?;
            if let Err(e) = std::fs::rename(&new_app, bundle) {
                let _ = std::fs::rename(&old, bundle);
                Err(format!("cannot put the new app in place: {e}"))
            } else {
                Ok(Relaunch::Bundle(bundle.clone()))
            }
        }
        None => {
            // Running a bare binary (e.g. from a terminal): replace just the executable.
            let tmp = exe.with_extension("new");
            std::fs::copy(new_app.join("Contents/MacOS/crossdesk"), &tmp).map_err(|e| e.to_string())?;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
            std::fs::rename(&tmp, &exe).map_err(|e| e.to_string())?;
            Ok(Relaunch::Exe(exe.clone()))
        }
    };
    let _ = std::fs::remove_dir_all(&work);
    result
}

#[cfg(not(any(windows, target_os = "macos")))]
fn install(_bytes: &[u8]) -> Result<Relaunch, String> {
    Err("updates are not supported on this OS".into())
}
