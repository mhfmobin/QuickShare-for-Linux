//! Finished transfers, kept across restarts in ~/.local/share/quickshare-linux/history.ini.

use std::path::PathBuf;

use gtk::glib;

/// Older entries are dropped.
const KEEP: usize = 50;

#[derive(Clone)]
pub struct Entry {
    /// Unix time
    pub time: i64,
    pub outgoing: bool,
    /// Pango markup
    pub title: String,
    /// Bytes; 0 hides it
    pub size: u64,
    /// Received: the saved files and folders. Sent: what was sent, for "Send again".
    pub files: Vec<PathBuf>,
    /// The text or link, when that's all the share was
    pub text: Option<String>,
}

fn path() -> PathBuf {
    glib::user_data_dir()
        .join("quickshare-linux")
        .join("history.ini")
}

/// Oldest first.
pub fn load() -> Vec<Entry> {
    let kf = glib::KeyFile::new();
    if kf.load_from_file(path(), glib::KeyFileFlags::NONE).is_err() {
        return vec![];
    }
    kf.groups()
        .iter()
        .filter_map(|g| {
            Some(Entry {
                time: kf.int64(g, "time").ok()?,
                outgoing: kf.boolean(g, "outgoing").ok()?,
                title: kf.string(g, "title").ok()?.into(),
                size: kf.uint64(g, "size").unwrap_or(0),
                files: (0..)
                    .map_while(|j| kf.string(g, &format!("file{j}")).ok())
                    .map(|s| PathBuf::from(s.as_str()))
                    .collect(),
                text: kf.string(g, "text").ok().map(Into::into),
            })
        })
        .collect()
}

pub fn save(entries: &[Entry]) {
    let kf = glib::KeyFile::new();
    for (i, e) in entries.iter().enumerate() {
        let g = i.to_string();
        kf.set_int64(&g, "time", e.time);
        kf.set_boolean(&g, "outgoing", e.outgoing);
        kf.set_string(&g, "title", &e.title);
        kf.set_uint64(&g, "size", e.size);
        for (j, f) in e.files.iter().enumerate() {
            kf.set_string(&g, &format!("file{j}"), &f.to_string_lossy());
        }
        if let Some(t) = &e.text {
            kf.set_string(&g, "text", t);
        }
    }
    let p = path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Err(e) = kf.save_to_file(&p) {
        glib::g_warning!("quickshare", "could not save history: {e}");
    }
}

pub fn add(entries: &mut Vec<Entry>, e: Entry) {
    entries.push(e);
    if entries.len() > KEEP {
        entries.drain(..entries.len() - KEEP);
    }
    save(entries);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("qs-history-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir); // read by glib on first use
        let e = Entry {
            time: 1_760_000_000,
            outgoing: false,
            title: "Received <b>a [b]; c</b>".into(),
            size: 42,
            files: vec!["/tmp/a;b/c d.png".into(), "/tmp/x=y".into()],
            text: Some("line 1\nline 2".into()),
        };
        let mut list = vec![];
        for i in 0..KEEP + 5 {
            add(
                &mut list,
                Entry {
                    time: i as i64,
                    ..e.clone()
                },
            );
        }
        let back = load();
        assert_eq!(back.len(), KEEP);
        assert_eq!(back[0].time, 5);
        let b = &back[KEEP - 1];
        assert_eq!(
            (&b.title, b.size, &b.files, &b.text),
            (&e.title, e.size, &e.files, &e.text)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
