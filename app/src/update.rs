//! Finds newer releases on GitHub, downloads the .deb and offers to install it from a banner.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{output, Ui};

const REPO: &str = env!("CARGO_PKG_REPOSITORY");
pub const EXE: &str = "/usr/bin/quickshare-linux";

/// Only the installed .deb updates itself; dev builds would always look outdated.
pub fn enabled() -> bool {
    std::env::current_exe().is_ok_and(|e| e == std::path::Path::new(EXE))
}

fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().split('.').map(|p| p.parse().ok());
    let t = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(t)
}

fn newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

fn dir() -> PathBuf {
    glib::user_cache_dir().join("quickshare-linux").join("updates")
}

/// Run a command to the end; Err holds its most telling stderr line.
async fn run(argv: &[&str]) -> Result<(), String> {
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let proc = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_PIPE)
        .map_err(|e| e.to_string())?;
    let (_, err) = proc
        .communicate_utf8_future(None)
        .await
        .map_err(|e| e.to_string())?;
    if proc.is_successful() {
        return Ok(());
    }
    let err = err.unwrap_or_default();
    let line = err.lines().find(|l| l.starts_with("E:")).or(err.lines().last());
    Err(line.unwrap_or_default().trim().to_string())
}

/// The release's .deb in the cache, downloaded unless already there.
async fn download(version: &str) -> Result<PathBuf, String> {
    let name = format!("QuickShare-for-linux-amd64-v{version}.deb");
    let path = dir().join(&name);
    if path.exists() {
        return Ok(path);
    }
    // drop older downloads that were never installed
    let _ = std::fs::remove_dir_all(dir());
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    let part = path.with_extension("part");
    let url = format!("{REPO}/releases/download/v{version}/{name}");
    run(&["curl", "-fsSL", "-o", &part.to_string_lossy(), &url]).await?;
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

fn offer(ui: &Ui, title: &str) {
    ui.banner.set_title(title);
    ui.banner.set_button_label(Some("Install"));
    ui.banner.set_revealed(true);
}

/// Look for a newer release; download it if the user allows, then show the banner.
pub fn check(ui: &Rc<Ui>) {
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        // /releases/latest redirects to /releases/tag/vX.Y.Z
        let latest_url = format!("{REPO}/releases/latest");
        let argv = ["curl", "-fsSL", "-o", "/dev/null", "-w", "%{url_effective}", &latest_url];
        let Some(url) = output(&argv).await else { return };
        let Some((_, latest)) = url.trim().rsplit_once("/v") else { return };
        // nothing new, already offered, or installed and waiting for a restart
        let shown = ui.banner.is_revealed()
            && ui.update.borrow().as_deref().is_none_or(|v| !newer(latest, v));
        if shown || !newer(latest, env!("CARGO_PKG_VERSION")) {
            return;
        }
        *ui.update.borrow_mut() = Some(latest.to_string());
        let auto = ui.st.borrow().prefs.auto_update;
        if auto && download(latest).await.is_ok() {
            offer(&ui, &format!("Version {latest} is ready to install"));
        } else {
            offer(&ui, &format!("Version {latest} is available"));
        }
        if !ui.win.is_visible() {
            let n = gio::Notification::new("Update available");
            n.set_body(Some(&format!("QuickShare for Linux {latest} is ready to install.")));
            n.set_default_action("app.show-window");
            ui.app.send_notification(Some("update"), &n);
        }
    });
}

/// Banner button: download if needed and install, or restart once installed.
pub fn on_button(ui: &Rc<Ui>) {
    let Some(version) = ui.update.borrow().clone() else {
        crate::RESTART.with(|c| c.set(true));
        ui.app.quit();
        return;
    };
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        ui.banner.set_button_label(None);
        ui.banner.set_title("Downloading update…");
        let res = match download(&version).await {
            Ok(path) => {
                ui.banner.set_title("Installing update…");
                run(&["pkexec", "apt-get", "install", "-y", &path.to_string_lossy()]).await
            }
            Err(e) => Err(e),
        };
        match res {
            Ok(()) => {
                *ui.update.borrow_mut() = None;
                let _ = std::fs::remove_dir_all(dir());
                ui.banner.set_title(&format!("Updated to version {version}"));
                ui.banner.set_button_label(Some("Restart"));
            }
            Err(e) => {
                offer(&ui, &format!("Version {version} is available"));
                ui.toast(&if e.is_empty() {
                    "Update not installed".to_string()
                } else {
                    format!("Update failed: {e}")
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions() {
        assert!(newer("1.0.10", "1.0.9"));
        assert!(newer("2.0.0", "1.9.9"));
        assert!(!newer("1.0.0", "1.0.0"));
        assert!(!newer("0.9.0", "1.0.0"));
        assert!(!newer("garbage", "1.0.0"));
        assert!(!newer("1.0", "0.1.0"));
    }
}
