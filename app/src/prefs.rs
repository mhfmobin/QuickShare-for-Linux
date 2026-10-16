//! Settings in a plain key file (~/.config/quickshare-linux/settings.ini): works uninstalled and in Flatpak.

use std::path::PathBuf;

use gtk::glib;

pub const APP_ID: &str = "io.github.mhfmobin.QuickShareLinux";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    Everyone,
    Temporary,
    Hidden,
}

impl Visibility {
    pub fn key(self) -> &'static str {
        match self {
            Visibility::Everyone => "everyone",
            Visibility::Temporary => "temporary",
            Visibility::Hidden => "hidden",
        }
    }
    pub fn from_key(s: &str) -> Self {
        match s {
            "temporary" => Visibility::Temporary,
            "hidden" => Visibility::Hidden,
            _ => Visibility::Everyone,
        }
    }
}

#[derive(Clone)]
pub struct Prefs {
    pub name: String,
    pub download_dir: PathBuf,
    pub trusted: Vec<String>,
    pub background: bool,
    pub autostart: bool,
    pub start_visibility: Visibility,
    pub port: u16,
    /// The port the user said their firewall allows (hides the firewall hint).
    pub firewall_ok_port: u16,
    /// Download new releases in the background (the banner shows either way).
    pub auto_update: bool,
}

pub const DEFAULT_PORT: u16 = 47130;

fn path() -> PathBuf {
    glib::user_config_dir()
        .join("quickshare-linux")
        .join("settings.ini")
}

fn autostart_path() -> PathBuf {
    glib::user_config_dir()
        .join("autostart")
        .join(format!("{APP_ID}.desktop"))
}

impl Prefs {
    pub fn load() -> Self {
        let kf = glib::KeyFile::new();
        let _ = kf.load_from_file(path(), glib::KeyFileFlags::NONE);
        let s = |k: &str| kf.string("main", k).ok().map(|v| v.to_string());
        let b = |k: &str, d: bool| kf.boolean("main", k).unwrap_or(d);
        Prefs {
            name: s("name")
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| glib::host_name().to_string()),
            download_dir: s("download_dir")
                .map(PathBuf::from)
                .or_else(|| glib::user_special_dir(glib::UserDirectory::Downloads))
                .unwrap_or_else(|| glib::home_dir().join("Downloads")),
            trusted: kf
                .keys("trusted")
                .map(|keys| {
                    keys.iter()
                        .filter_map(|k| kf.string("trusted", k).ok())
                        .map(|v| v.to_string())
                        .collect()
                })
                .unwrap_or_default(),
            background: b("background", true),
            autostart: autostart_path().exists(),
            start_visibility: Visibility::from_key(&s("start_visibility").unwrap_or_default()),
            port: kf
                .integer("main", "port")
                .ok()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|&p| p >= 1024)
                .unwrap_or(DEFAULT_PORT),
            firewall_ok_port: kf.integer("main", "firewall_ok_port").unwrap_or(0) as u16,
            auto_update: b("auto_update", true),
        }
    }

    pub fn save(&self) {
        let kf = glib::KeyFile::new();
        kf.set_string("main", "name", &self.name);
        kf.set_string("main", "download_dir", &self.download_dir.to_string_lossy());
        for (i, name) in self.trusted.iter().enumerate() {
            kf.set_string("trusted", &i.to_string(), name);
        }
        kf.set_boolean("main", "background", self.background);
        kf.set_string("main", "start_visibility", self.start_visibility.key());
        kf.set_integer("main", "port", self.port.into());
        kf.set_integer("main", "firewall_ok_port", self.firewall_ok_port.into());
        kf.set_boolean("main", "auto_update", self.auto_update);
        let p = path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Err(e) = kf.save_to_file(&p) {
            glib::g_warning!("quickshare", "could not save settings: {e}");
        }
    }

    /// Start at login via an XDG autostart entry (Flatpak builds will use the Background portal instead).
    pub fn set_autostart(&mut self, on: bool) {
        let p = autostart_path();
        self.autostart = on;
        if !on {
            let _ = std::fs::remove_file(&p);
            return;
        }
        let exe = std::env::current_exe()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "quickshare-linux".into());
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=QuickShare for Linux\nExec=\"{exe}\" --background\nIcon={APP_ID}\nX-GNOME-Autostart-enabled=true\n"
        );
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Err(e) = std::fs::write(&p, entry) {
            glib::g_warning!("quickshare", "could not enable autostart: {e}");
            self.autostart = false;
        }
    }
}
