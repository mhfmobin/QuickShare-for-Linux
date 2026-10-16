mod dialogs;
mod prefs;
mod radar;
mod update;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use qs_core::{
    Device, DeviceKind, Event, Incoming, Medium, Outcome, Payload, Route, TextKind, TransferId,
};

use prefs::{Prefs, Visibility, APP_ID};
use radar::Radar;

thread_local! {
    static START_HIDDEN: Cell<bool> = const { Cell::new(false) };
    /// Start the updated binary once the app has shut down.
    static RESTART: Cell<bool> = const { Cell::new(false) };
}

pub enum Staged {
    Files(Vec<PathBuf>),
    Text(String),
}

struct DevNode {
    dev: Device,
    button: gtk::Button,
    progress: Rc<Cell<f64>>,
    ring: gtk::DrawingArea,
}

struct FeedRow {
    badge: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    meta: gtk::Label,
    bar: gtk::ProgressBar,
    acts: gtk::Box,
}

struct Tx {
    row: FeedRow,
    outgoing: bool,
    peer: String,
    device: Option<String>,
    what: String,
    pin: Option<String>,
    accepted: bool,
    started: Instant,
    done: u64,
    total: u64,
    medium: Option<Medium>,
}

pub struct State {
    pub prefs: Prefs,
    devices: Vec<DevNode>,
    staged: Option<Staged>,
    transfers: HashMap<TransferId, Tx>,
    pending: HashMap<TransferId, (Incoming, Option<adw::AlertDialog>)>,
    vis_until: Option<Instant>,
    hold: Option<gio::ApplicationHoldGuard>,
}

pub struct Ui {
    pub app: adw::Application,
    pub win: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    /// Newer release offered in the banner; None once installed.
    update: RefCell<Option<String>>,
    radar: Radar,
    center_k: gtk::Label,
    center_big: gtk::Label,
    center_sub: gtk::Label,
    tray: gtk::Box,
    tray_icon: gtk::Image,
    tray_label: gtk::Label,
    tray_chips: gtk::Box,
    tray_btn: gtk::Button,
    checks: gtk::ListBox,
    net_row: adw::ActionRow,
    net_pill: gtk::Label,
    /// Why a VPN may be blocking nearby devices, if one is on.
    vpn: RefCell<Option<String>>,
    /// Command that opens the firewall for us, when a firewall is on.
    firewall: RefCell<Option<String>>,
    fw_row: adw::ActionRow,
    /// No other device has answered over mDNS for a while.
    isolated: Cell<bool>,
    started: Instant,
    bt_row: adw::ActionRow,
    bt_pill: gtk::Label,
    bt_btn: gtk::Button,
    feed: gtk::ListBox,
    vis_label: gtk::Label,
    vis_dot: gtk::Box,
    pub core: qs_core::Handle,
    pub st: RefCell<State>,
}

fn esc(s: &str) -> glib::GString {
    glib::markup_escape_text(s)
}

fn size(b: u64) -> String {
    glib::format_size(b).to_string()
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// Shown after a transfer's progress.
fn medium_note(m: Option<Medium>) -> &'static str {
    match m {
        Some(Medium::Bluetooth) => " · over Bluetooth (slow)",
        Some(Medium::Direct) => " · over the phone's Wi‑Fi",
        _ => "",
    }
}

fn kind_name(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::Phone => "Phone",
        DeviceKind::Tablet => "Tablet",
        DeviceKind::Laptop => "Computer",
        DeviceKind::Unknown => "Device",
    }
}

fn kind_icon(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::Tablet => "tablet-symbolic",
        DeviceKind::Laptop => "computer-symbolic",
        _ => "phone-symbolic",
    }
}

pub fn mime_icon(mime: &str) -> &'static str {
    match mime.split('/').next() {
        Some("image") => "image-x-generic-symbolic",
        Some("video") => "video-x-generic-symbolic",
        Some("audio") => "audio-x-generic-symbolic",
        _ => "x-office-document-symbolic",
    }
}

fn describe_staged(s: &Staged) -> String {
    match s {
        Staged::Files(p) if p.len() == 1 => p[0]
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        Staged::Files(p) => format!("{} files", p.len()),
        Staged::Text(t) if is_url(t) => "a link".into(),
        Staged::Text(_) => "some text".into(),
    }
}

pub fn is_url(t: &str) -> bool {
    t.starts_with("https://") || t.starts_with("http://")
}

pub fn describe_incoming(req: &Incoming) -> String {
    if let Some(top) = req.files.first().and_then(|f| f.folder.first()) {
        if req.texts.is_empty() && req.files.iter().all(|f| f.folder.first() == Some(top)) {
            return format!("the folder “{top}”");
        }
    }
    match (req.texts.as_slice(), req.files.len()) {
        ([(_, TextKind::Url)], 0) => "a link".into(),
        ([_], 0) => "some text".into(),
        ([], 1) => req.files[0].name.clone(),
        ([], n) => format!("{n} files"),
        (t, n) => format!("{} items", t.len() + n),
    }
}

/// Bytes in a file, or in everything under a folder (symlinks inside are skipped, like the sender does).
fn tree_size(p: &std::path::Path) -> u64 {
    match p.metadata() {
        Ok(m) if m.is_dir() => std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| !t.is_symlink()))
            .map(|e| tree_size(&e.path()))
            .sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn duration_left(secs: u64) -> String {
    match secs {
        0 => "almost done".into(),
        1..=59 => format!("{secs} s left"),
        _ => format!("{} min left", secs.div_ceil(60)),
    }
}

impl FeedRow {
    fn set(&self, kind: &str, title: &str, meta: &str) {
        for k in ["in", "out", "no"] {
            self.badge.remove_css_class(k);
        }
        self.badge.add_css_class(kind);
        self.icon.set_icon_name(Some(match kind {
            "in" => "folder-download-symbolic",
            "out" => "send-to-symbolic",
            _ => "window-close-symbolic",
        }));
        self.title.set_markup(title);
        self.meta.set_text(meta);
    }

    fn finish(&self, kind: &str, title: &str, meta: &str) {
        self.set(kind, title, meta);
        self.bar.set_visible(false);
        while let Some(c) = self.acts.first_child() {
            self.acts.remove(&c);
        }
        self.acts.set_visible(false);
    }

    fn action(&self, text: &str, f: impl Fn() + 'static) -> gtk::Button {
        let b = gtk::Button::with_label(text);
        b.add_css_class("flat");
        b.add_css_class("feed-action");
        b.connect_clicked(move |_| f());
        self.acts.append(&b);
        self.acts.set_visible(true);
        b
    }
}

impl Ui {
    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn hold(&self) {
        let mut st = self.st.borrow_mut();
        if st.hold.is_none() {
            st.hold = Some(self.app.hold());
        }
    }

    fn feed_row(&self, kind: &str, title: &str, meta: &str) -> FeedRow {
        let icon = gtk::Image::new();
        icon.set_hexpand(true);
        let badge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        badge.add_css_class("badge");
        badge.set_valign(gtk::Align::Start);
        badge.append(&icon);
        let title_l = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        title_l.add_css_class("feed-title");
        let meta_l = gtk::Label::builder().wrap(true).xalign(0.0).build();
        meta_l.add_css_class("dim-label");
        meta_l.add_css_class("caption");
        let bar = gtk::ProgressBar::new();
        bar.set_visible(false);
        bar.add_css_class("feed-bar");
        let acts = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        acts.set_visible(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_hexpand(true);
        for w in [
            title_l.upcast_ref::<gtk::Widget>(),
            meta_l.upcast_ref(),
            bar.upcast_ref(),
            acts.upcast_ref(),
        ] {
            content.append(w);
        }
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.append(&badge);
        hbox.append(&content);
        let row = gtk::ListBoxRow::builder()
            .child(&hbox)
            .activatable(false)
            .build();
        row.add_css_class("feed-row");
        self.feed.prepend(&row);
        let r = FeedRow {
            badge,
            icon,
            title: title_l,
            meta: meta_l,
            bar,
            acts,
        };
        r.set(kind, title, meta);
        r
    }

    // ---- staging & sending ----

    fn stage(&self, s: Staged) {
        self.st.borrow_mut().staged = Some(s);
        self.refresh();
    }

    fn choose_files(self: &Rc<Self>, target: Option<String>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose files to send")
            .modal(true)
            .build();
        let ui = self.clone();
        dialog.open_multiple(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            let Ok(list) = res else { return };
            let paths: Vec<PathBuf> = (0..list.n_items())
                .filter_map(|i| list.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
                .collect();
            if paths.is_empty() {
                return;
            }
            match target {
                Some(id) => ui.start_send(&id, Staged::Files(paths)),
                None => ui.stage(Staged::Files(paths)),
            }
        });
    }

    fn send_to(self: &Rc<Self>, dev_id: &str) {
        let staged = self.st.borrow_mut().staged.take();
        match staged {
            Some(s) => self.start_send(dev_id, s),
            None => self.choose_files(Some(dev_id.to_string())),
        }
    }

    fn start_send(self: &Rc<Self>, dev_id: &str, staged: Staged) {
        let Some(dev) = self
            .st
            .borrow()
            .devices
            .iter()
            .find(|n| n.dev.id == dev_id)
            .map(|n| n.dev.clone())
        else {
            self.toast("That device is no longer nearby");
            return;
        };
        let what = describe_staged(&staged);
        let total = match &staged {
            Staged::Files(p) => p.iter().map(|p| tree_size(p)).sum(),
            Staged::Text(t) => t.len() as u64,
        };
        let payload = match staged {
            Staged::Files(files) => Payload {
                files,
                ..Default::default()
            },
            Staged::Text(t) => Payload {
                texts: vec![t],
                ..Default::default()
            },
        };
        let bluetooth = matches!(dev.route, Route::Bluetooth(_));
        let id = self.core.send(dev.clone(), payload);
        let row = self.feed_row(
            "out",
            &format!("Sending <b>{}</b> to {}", esc(&what), esc(&dev.name)),
            if bluetooth {
                "Connecting over Bluetooth… For speed, this PC may briefly join the phone's Wi‑Fi."
            } else {
                "Connecting…"
            },
        );
        row.bar.set_visible(true);
        let ui = self.clone();
        row.action("Cancel", move || ui.core.cancel(id));
        self.st.borrow_mut().transfers.insert(
            id,
            Tx {
                row,
                outgoing: true,
                peer: dev.name.clone(),
                device: Some(dev.id.clone()),
                what,
                pin: None,
                accepted: false,
                started: Instant::now(),
                done: 0,
                total,
                medium: None,
            },
        );
        self.refresh();
    }

    fn paste(self: &Rc<Self>) {
        let cb = self.win.clipboard();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if cb.formats().contains_type(gdk::FileList::static_type()) {
                if let Ok(v) = cb
                    .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                    .await
                {
                    if let Ok(list) = v.get::<gdk::FileList>() {
                        let paths: Vec<PathBuf> =
                            list.files().iter().filter_map(|f| f.path()).collect();
                        if !paths.is_empty() {
                            ui.stage(Staged::Files(paths));
                            return;
                        }
                    }
                }
            }
            match cb.read_text_future().await {
                Ok(Some(t)) if !t.trim().is_empty() => ui.stage(Staged::Text(t.trim().to_string())),
                _ => ui.toast("Nothing to send on the clipboard"),
            }
        });
    }

    // ---- devices ----

    fn add_device(self: &Rc<Self>, dev: Device) {
        let mut st = self.st.borrow_mut();
        if let Some(n) = st.devices.iter_mut().find(|n| n.dev.id == dev.id) {
            // a LAN route beats Bluetooth
            if !(matches!(n.dev.route, Route::Lan(_)) && matches!(dev.route, Route::Bluetooth(_))) {
                n.dev = dev;
            }
            return;
        }
        let bluetooth = matches!(dev.route, Route::Bluetooth(_));
        let progress = Rc::new(Cell::new(0.0f64));
        let ring = gtk::DrawingArea::new();
        ring.add_css_class("progress-ring");
        ring.set_can_target(false);
        let p = progress.clone();
        ring.set_draw_func(move |area, cr, w, h| {
            let v = p.get();
            if v <= 0.0 {
                return;
            }
            let c = area.color();
            let (cx, cy, r) = (
                w as f64 / 2.0,
                h as f64 / 2.0,
                (w.min(h) as f64) / 2.0 - 2.0,
            );
            cr.set_line_width(2.5);
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.15);
            cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 1.0);
            cr.set_line_cap(gtk::cairo::LineCap::Round);
            let start = -std::f64::consts::FRAC_PI_2;
            cr.arc(cx, cy, r, start, start + std::f64::consts::TAU * v.min(1.0));
            let _ = cr.stroke();
        });
        let icon = gtk::Image::from_icon_name(kind_icon(dev.kind));
        icon.set_pixel_size(24);
        let disc = gtk::Overlay::new();
        disc.add_css_class("disc");
        disc.set_halign(gtk::Align::Center);
        disc.set_size_request(58, 58);
        disc.set_child(Some(&icon));
        disc.add_overlay(&ring);
        let name = label(&dev.name, &["name"]);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(14);
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
        vbox.append(&disc);
        vbox.append(&name);
        let kind = if bluetooth {
            format!("{} · Bluetooth", kind_name(dev.kind))
        } else {
            kind_name(dev.kind).to_string()
        };
        vbox.append(&label(&kind, &["kind", "dim-label"]));
        let button = gtk::Button::builder()
            .child(&vbox)
            .tooltip_text(if bluetooth {
                format!("Send to {} over Bluetooth. For speed, this PC may briefly join the phone's Wi‑Fi.", dev.name)
            } else {
                format!("Send to {}", dev.name)
            })
            .build();
        button.add_css_class("node");
        button.add_css_class("flat");
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "Send to {}, {}",
            dev.name,
            kind_name(dev.kind)
        ))]);
        let ui = self.clone();
        let id = dev.id.clone();
        button.connect_clicked(move |_| ui.send_to(&id));
        self.radar.add_node(&button);
        st.devices.push(DevNode {
            dev,
            button,
            progress,
            ring,
        });
        drop(st);
        self.refresh();
    }

    fn remove_device(&self, id: &str) {
        let mut st = self.st.borrow_mut();
        // keep the node while we're sending to it
        if st
            .transfers
            .values()
            .any(|t| t.device.as_deref() == Some(id))
        {
            return;
        }
        if let Some(i) = st.devices.iter().position(|n| n.dev.id == id) {
            let n = st.devices.remove(i);
            self.radar.remove_node(&n.button);
        }
        drop(st);
        self.refresh();
    }

    // ---- incoming ----

    fn on_incoming(self: &Rc<Self>, req: Incoming) {
        let (id, from, what) = (req.id, req.from.clone(), describe_incoming(&req));
        let trusted = self.st.borrow().prefs.trusted.contains(&from);
        if trusted {
            let medium = req.medium;
            req.answer(true);
            self.begin_receive(id, &from, &what, medium);
            if !self.win.is_visible() {
                let n = gio::Notification::new(&format!("Receiving {what}"));
                n.set_body(Some(&format!("From {from}")));
                n.set_default_action("app.show-window");
                self.app.send_notification(Some(&format!("in-{id}")), &n);
            }
            return;
        }
        self.st.borrow_mut().pending.insert(id, (req, None));
        if self.win.is_visible() {
            dialogs::incoming(self, id);
        } else {
            let n = gio::Notification::new(&format!("{from} wants to share {what}"));
            n.set_body(Some(
                "Open QuickShare for Linux to check the PIN, or answer here.",
            ));
            n.set_priority(gio::NotificationPriority::High);
            n.add_button_with_target_value("Decline", "app.decline", Some(&id.to_variant()));
            n.add_button_with_target_value("Accept", "app.accept", Some(&id.to_variant()));
            n.set_default_action_and_target_value("app.show-incoming", Some(&id.to_variant()));
            self.app.send_notification(Some(&format!("in-{id}")), &n);
        }
    }

    pub fn answer(self: &Rc<Self>, id: TransferId, accept: bool, trust: bool) {
        let Some((req, _)) = self.st.borrow_mut().pending.remove(&id) else {
            return;
        };
        self.app.withdraw_notification(&format!("in-{id}"));
        let (from, what) = (req.from.clone(), describe_incoming(&req));
        if accept && trust {
            let mut st = self.st.borrow_mut();
            if !st.prefs.trusted.contains(&from) {
                st.prefs.trusted.push(from.clone());
                st.prefs.save();
            }
        }
        if accept {
            self.begin_receive(id, &from, &what, req.medium);
        } else {
            self.feed_row(
                "no",
                &format!("Declined {} from <b>{}</b>", esc(&what), esc(&from)),
                "Just now",
            );
        }
        req.answer(accept);
    }

    fn begin_receive(self: &Rc<Self>, id: TransferId, from: &str, what: &str, medium: Medium) {
        let row = self.feed_row(
            "in",
            &format!("Receiving <b>{}</b> from {}", esc(what), esc(from)),
            "Starting…",
        );
        row.bar.set_visible(true);
        let ui = self.clone();
        row.action("Cancel", move || ui.core.cancel(id));
        self.st.borrow_mut().transfers.insert(
            id,
            Tx {
                row,
                outgoing: false,
                peer: from.into(),
                device: None,
                what: what.into(),
                pin: None,
                accepted: true,
                started: Instant::now(),
                done: 0,
                total: 0,
                medium: Some(medium),
            },
        );
        self.refresh();
    }

    pub fn show_pending(self: &Rc<Self>, id: TransferId) {
        self.win.present();
        let has_dialog = self.st.borrow().pending.get(&id).map(|(_, d)| d.is_some());
        if has_dialog == Some(false) {
            dialogs::incoming(self, id);
        }
    }

    pub fn attach_dialog(&self, id: TransferId, dlg: adw::AlertDialog) {
        if let Some(entry) = self.st.borrow_mut().pending.get_mut(&id) {
            entry.1 = Some(dlg);
        }
    }

    fn open_file(&self, path: PathBuf, folder: bool) {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
        if folder {
            launcher.open_containing_folder(Some(&self.win), None::<&gio::Cancellable>, |_| {});
        } else {
            launcher.launch(Some(&self.win), None::<&gio::Cancellable>, |_| {});
        }
    }

    // ---- core events ----

    fn on_event(self: &Rc<Self>, ev: Event) {
        match ev {
            Event::DeviceFound(d) => self.add_device(d),
            Event::DeviceLost(id) => self.remove_device(&id),
            Event::Incoming(req) => self.on_incoming(req),
            Event::Connected { id, pin } => {
                if let Some(tx) = self.st.borrow_mut().transfers.get_mut(&id) {
                    tx.row
                        .meta
                        .set_text(&format!("Waiting for {} to accept · PIN {pin}", tx.peer));
                    tx.pin = Some(pin);
                }
                self.refresh();
            }
            Event::Accepted { id } => {
                if let Some(tx) = self.st.borrow_mut().transfers.get_mut(&id) {
                    tx.accepted = true;
                    tx.started = Instant::now();
                    tx.row.meta.set_text("Starting…");
                }
                self.refresh();
            }
            Event::Medium { id, medium } => {
                let mut st = self.st.borrow_mut();
                if let Some(tx) = st.transfers.get_mut(&id) {
                    if tx.medium == Some(Medium::Bluetooth) && medium == Medium::Direct {
                        tx.started = Instant::now(); // fresh speed estimate on the fast link
                        tx.row
                            .meta
                            .set_text("Switched to the phone's Wi‑Fi for speed…");
                    }
                    tx.medium = Some(medium);
                }
            }
            Event::WifiOff { id } => {
                let btn = self.st.borrow().transfers.get(&id).map(|tx| {
                    tx.row
                        .meta
                        .set_text("Wi‑Fi is off, so this goes over slow Bluetooth");
                    // stays on the row, to bring the question back after "Keep Bluetooth"
                    let b = tx.row.action("Turn on Wi‑Fi…", || {});
                    let ui = self.clone();
                    b.connect_clicked(move |b| dialogs::wifi_off(&ui, b));
                    b
                });
                if let Some(b) = btn {
                    dialogs::wifi_off(self, &b);
                }
            }
            Event::Progress { id, done, total } => self.on_progress(id, done, total),
            Event::Done { id, outcome } => self.on_done(id, outcome),
            Event::Bluetooth(on) => {
                let missing = !on && !has_adapter("/sys/class/bluetooth", "");
                self.bt_row.set_title(match (on, missing) {
                    (true, _) => "Bluetooth",
                    (_, true) => "No Bluetooth adapter",
                    _ => "Bluetooth is off",
                });
                self.bt_row.set_subtitle(if on {
                    "On. Phones set to Everyone show up here."
                } else if missing {
                    "This PC has no Bluetooth, so phones are found over the network only."
                } else {
                    "Turn it on to find phones to send to. Receiving still works over Wi‑Fi."
                });
                self.bt_pill.set_visible(on);
                self.bt_btn.set_visible(!on && !missing);
            }
        }
    }

    fn on_progress(&self, id: TransferId, done: u64, total: u64) {
        {
            let st = &mut *self.st.borrow_mut();
            let Some(tx) = st.transfers.get_mut(&id) else {
                return;
            };
            tx.done = done;
            tx.total = total.max(1);
            let frac = done as f64 / tx.total as f64;
            tx.row.bar.set_fraction(frac);
            let secs = tx.started.elapsed().as_secs_f64().max(0.1);
            let rate = (done as f64 / secs) as u64;
            let left = total.saturating_sub(done).checked_div(rate).unwrap_or(0);
            tx.row.meta.set_text(&format!(
                "{} of {} · {}/s · {}{}",
                size(done),
                size(total),
                size(rate),
                duration_left(left),
                medium_note(tx.medium)
            ));
            if let Some(n) = tx
                .device
                .as_ref()
                .and_then(|d| st.devices.iter().find(|n| &n.dev.id == d))
            {
                n.progress.set(frac);
                n.ring.queue_draw();
            }
        } // state borrow ends before refresh_center borrows again
        self.refresh_center();
    }

    fn on_done(self: &Rc<Self>, id: TransferId, outcome: Outcome) {
        let pending = self.st.borrow_mut().pending.remove(&id);
        if let Some((req, dlg)) = pending {
            self.app.withdraw_notification(&format!("in-{id}"));
            if let Some(d) = dlg {
                d.force_close();
            }
            self.toast(&format!("{} cancelled the share", req.from));
            return;
        }
        let Some(tx) = self.st.borrow_mut().transfers.remove(&id) else {
            return;
        };
        if let Some(d) = &tx.device {
            if let Some(n) = self.st.borrow().devices.iter().find(|n| &n.dev.id == d) {
                n.progress.set(0.0);
                n.ring.queue_draw();
            }
        }
        let (peer, what) = (esc(&tx.peer), esc(&tx.what));
        let now = "Just now".to_string();
        let total = if tx.total > 0 {
            format!(" · {}", size(tx.total))
        } else {
            String::new()
        };
        match outcome {
            Outcome::Sent => {
                tx.row.finish(
                    "out",
                    &format!("Sent <b>{what}</b> to {peer}"),
                    &(now + &total),
                );
                self.toast(&format!("Sent to {}", tx.peer));
            }
            Outcome::Received { files, texts } => {
                let meta = if files.is_empty() { now } else { now + &total };
                tx.row
                    .finish("in", &format!("Received <b>{what}</b> from {peer}"), &meta);
                if let Some(first) = files.first().cloned() {
                    let (ui, f) = (self.clone(), first.clone());
                    let single = files.len() == 1;
                    tx.row
                        .action("Open", move || ui.open_file(f.clone(), !single));
                    let (ui, f) = (self.clone(), first.clone());
                    tx.row
                        .action("Show in folder", move || ui.open_file(f.clone(), true));
                    let toast = adw::Toast::builder()
                        .title(if single {
                            "Saved to Downloads".to_string()
                        } else {
                            format!("Saved {} items", files.len())
                        })
                        .button_label("Show")
                        .build();
                    let (ui, f) = (self.clone(), first.clone());
                    toast.connect_button_clicked(move |_| ui.open_file(f.clone(), true));
                    self.toasts.add_toast(toast);
                    if !self.win.is_visible() {
                        let n = gio::Notification::new(&format!(
                            "Received {} from {}",
                            tx.what, tx.peer
                        ));
                        n.set_default_action("app.show-window");
                        self.app.send_notification(Some(&format!("in-{id}")), &n);
                    }
                }
                if let ([t], true) = (texts.as_slice(), files.is_empty()) {
                    let (ui, t2) = (self.clone(), t.clone());
                    tx.row.action("Copy", move || {
                        ui.win.clipboard().set_text(&t2);
                        ui.toast("Copied");
                    });
                    if is_url(t) {
                        let (ui, t2) = (self.clone(), t.clone());
                        tx.row.action("Open", move || {
                            gtk::UriLauncher::new(&t2).launch(
                                Some(&ui.win),
                                None::<&gio::Cancellable>,
                                |_| {},
                            )
                        });
                    }
                }
                for (i, t) in texts.iter().enumerate() {
                    if self.win.is_visible() {
                        dialogs::text(self, &tx.peer, t);
                    } else {
                        let n = gio::Notification::new(&format!(
                            "{} from {}",
                            if is_url(t) { "Link" } else { "Text" },
                            tx.peer
                        ));
                        n.set_body(Some(t));
                        n.set_default_action("app.show-window");
                        self.app
                            .send_notification(Some(&format!("in-{id}-text{i}")), &n);
                    }
                }
            }
            Outcome::Rejected => {
                let title = if tx.outgoing {
                    format!("{peer} declined <b>{what}</b>")
                } else {
                    format!("Declined {what} from <b>{peer}</b>")
                };
                tx.row.finish("no", &title, &now);
                if tx.outgoing {
                    self.toast(&format!("{} declined", tx.peer));
                }
            }
            Outcome::Cancelled => tx
                .row
                .finish("no", &format!("Cancelled <b>{what}</b>"), &now),
            Outcome::Failed(reason) => {
                let title = if tx.outgoing {
                    format!("Couldn't send <b>{what}</b> to {peer}")
                } else {
                    format!("Couldn't receive <b>{what}</b> from {peer}")
                };
                tx.row.finish("no", &title, &reason);
                self.toast(&format!("Transfer failed: {reason}"));
            }
        }
        self.refresh();
    }

    // ---- visibility ----

    fn apply_visibility(self: &Rc<Self>, v: Visibility) {
        self.core.set_visible(v != Visibility::Hidden);
        let until = (v == Visibility::Temporary).then(|| Instant::now() + Duration::from_secs(600));
        self.st.borrow_mut().vis_until = until;
        if v == Visibility::Hidden {
            self.vis_dot.add_css_class("off");
        } else {
            self.vis_dot.remove_css_class("off");
        }
        self.vis_label.set_text(match v {
            Visibility::Everyone => "Visible to everyone",
            Visibility::Temporary => "Visible · 10:00",
            Visibility::Hidden => "Hidden",
        });
        if until.is_some() {
            let ui = self.clone();
            glib::timeout_add_seconds_local(1, move || {
                let Some(until) = ui.st.borrow().vis_until else {
                    return glib::ControlFlow::Break;
                };
                let left = until.saturating_duration_since(Instant::now()).as_secs();
                if left == 0 {
                    ui.app
                        .activate_action("visibility", Some(&"hidden".to_variant()));
                    return glib::ControlFlow::Break;
                }
                ui.vis_label
                    .set_text(&format!("Visible · {}:{:02}", left / 60, left % 60));
                glib::ControlFlow::Continue
            });
        }
    }

    // ---- view state ----

    fn refresh(&self) {
        let st = self.st.borrow();
        let has = st.staged.is_some();
        for c in [
            self.tray.upcast_ref::<gtk::Widget>(),
            self.radar.upcast_ref(),
        ] {
            if has {
                c.add_css_class("staged")
            } else {
                c.remove_css_class("staged")
            }
        }
        while let Some(c) = self.tray_chips.first_child() {
            self.tray_chips.remove(&c);
        }
        match &st.staged {
            None => {
                self.tray_icon
                    .set_icon_name(Some("x-office-document-symbolic"));
                self.tray_label
                    .set_markup("Drop files here or press <b>Ctrl+O</b>, then pick a device.");
                self.tray_btn.set_label("Choose files");
            }
            Some(Staged::Files(paths)) => {
                let total: u64 = paths.iter().map(|p| tree_size(p)).sum();
                let n = paths.len();
                self.tray_icon
                    .set_icon_name(Some("x-office-document-symbolic"));
                self.tray_label.set_markup(&format!(
                    "<b>{n} file{} ready</b> · {} · pick a device",
                    if n == 1 { "" } else { "s" },
                    size(total)
                ));
                for p in paths.iter().take(3) {
                    let chip = label(
                        &p.file_name().unwrap_or_default().to_string_lossy(),
                        &["chip"],
                    );
                    chip.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                    chip.set_max_width_chars(22);
                    self.tray_chips.append(&chip);
                }
                if n > 3 {
                    self.tray_chips
                        .append(&label(&format!("+{} more", n - 3), &["chip"]));
                }
                self.tray_btn.set_label("Clear");
            }
            Some(Staged::Text(t)) => {
                self.tray_icon.set_icon_name(Some(if is_url(t) {
                    "web-browser-symbolic"
                } else {
                    "edit-paste-symbolic"
                }));
                self.tray_label.set_markup(&format!(
                    "<b>{} ready</b> · pick a device",
                    if is_url(t) { "Link" } else { "Text" }
                ));
                let chip = label(t, &["chip"]);
                chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
                chip.set_max_width_chars(40);
                self.tray_chips.append(&chip);
                self.tray_btn.set_label("Clear");
            }
        }
        self.tray_chips.set_visible(has);
        self.checks.set_visible(st.devices.is_empty());
        let online = gio::NetworkMonitor::default().is_network_available();
        let vpn = self.vpn.borrow();
        let (sub, pill, ok) = if let Some(why) = &*vpn {
            (why.as_str(), "VPN", false)
        } else if !online {
            (
                if !has_adapter("/sys/class/net", "wireless") {
                    "No network and no Wi‑Fi adapter. Plug in Ethernet; phones can still send over Bluetooth."
                } else {
                    "Not connected. Join the same Wi‑Fi network as the phone."
                },
                "OFFLINE",
                false,
            )
        } else if self.isolated.get() {
            ("Connected, but no other device has answered yet. Guest and public Wi‑Fi often keep devices apart; a home network or a phone hotspot works. Phones nearby can still reach this PC over Bluetooth.", "CHECK", false)
        } else {
            (
                "Connected. The phone must be on the same Wi‑Fi network.",
                "READY",
                true,
            )
        };
        self.net_row.set_subtitle(sub);
        self.net_pill.set_text(pill);
        self.net_pill
            .set_css_classes(&["pill", if ok { "ok" } else { "warn" }]);
        let fw = self.firewall.borrow();
        self.fw_row
            .set_visible(fw.is_some() && st.prefs.firewall_ok_port != self.core.port());
        if let Some(cmd) = &*fw {
            self.fw_row.set_subtitle(&format!(
                "If the phone can't connect, allow QuickShare for Linux's port. Run: {cmd}"
            ));
        }
        self.radar
            .set_scanning(st.transfers.values().all(|t| !t.outgoing));
        drop(st);
        self.refresh_center();
    }

    fn refresh_center(&self) {
        let st = self.st.borrow();
        let active = st
            .transfers
            .iter()
            .filter(|(_, t)| t.outgoing)
            .min_by_key(|(id, _)| **id)
            .map(|(_, t)| t);
        let (k, big, sub, pin_style) = match active {
            Some(t) if !t.accepted => match &t.pin {
                Some(pin) => (
                    "CHECK THE PIN".to_string(),
                    esc(pin).to_string(),
                    format!("Waiting for {} to accept", t.peer),
                    true,
                ),
                None => (
                    "CONNECTING".into(),
                    "…".into(),
                    format!("Reaching {}", t.peer),
                    true,
                ),
            },
            Some(t) => {
                let pct = (t.done as f64 / t.total.max(1) as f64 * 100.0).round() as u32;
                (
                    format!("SENDING TO {}", t.peer.to_uppercase()),
                    format!("{pct}%"),
                    format!("{} of {}", size(t.done), size(t.total)),
                    true,
                )
            }
            None if st.staged.is_some() => {
                let n = st.devices.len();
                (
                    "THIS PC".into(),
                    "Send to <i>who?</i>".into(),
                    format!("{n} device{} nearby", if n == 1 { "" } else { "s" }),
                    false,
                )
            }
            None if st.devices.is_empty() => (
                "LOOKING".into(),
                "Nobody <i>nearby</i>".into(),
                "Searching your network".into(),
                false,
            ),
            None => (
                "THIS PC".into(),
                "Nearby".into(),
                format!("Phones see you as {}", st.prefs.name),
                false,
            ),
        };
        self.center_k.set_text(&k);
        self.center_big.set_markup(&big);
        self.center_sub.set_text(&sub);
        if pin_style {
            self.center_big.remove_css_class("display");
            self.center_big.add_css_class("pin");
        } else {
            self.center_big.remove_css_class("pin");
            self.center_big.add_css_class("display");
        }
    }
}

fn build(app: &adw::Application) -> Result<Rc<Ui>, String> {
    let prefs = Prefs::load();
    let (core, mut events) = qs_core::start(qs_core::Settings {
        name: prefs.name.clone(),
        download_dir: prefs.download_dir.clone(),
        port: prefs.port,
    })
    .map_err(|e| e.to_string())?;

    // header
    let vis_dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    vis_dot.add_css_class("vis-dot");
    vis_dot.set_valign(gtk::Align::Center);
    let vis_label = gtk::Label::new(None);
    vis_label.add_css_class("numeric");
    let vis_inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    vis_inner.append(&vis_dot);
    vis_inner.append(&vis_label);
    vis_inner.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    let vis_menu = gio::Menu::new();
    vis_menu.append(Some("Everyone"), Some("app.visibility::everyone"));
    vis_menu.append(
        Some("Everyone for 10 minutes"),
        Some("app.visibility::temporary"),
    );
    vis_menu.append(
        Some("Hidden (you can still send)"),
        Some("app.visibility::hidden"),
    );
    let vis_btn = gtk::MenuButton::builder()
        .child(&vis_inner)
        .menu_model(&vis_menu)
        .tooltip_text("Who can see this PC")
        .build();
    vis_btn.add_css_class("vis");

    let main_menu = gio::Menu::new();
    main_menu.append(Some("_Preferences"), Some("app.preferences"));
    main_menu.append(Some("_About QuickShare for Linux"), Some("app.about"));
    main_menu.append(Some("_Quit"), Some("app.quit"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&main_menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build();
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Choose files to send (Ctrl+O)")
        .action_name("win.open")
        .build();

    let header = adw::HeaderBar::new();
    header.pack_start(&vis_btn);
    header.set_title_widget(Some(&adw::WindowTitle::new("QuickShare for Linux", "")));
    header.pack_end(&menu_btn);
    header.pack_end(&add_btn);

    // stage
    let center_k = label("", &["eyebrow"]);
    let center_big = label("", &["display"]);
    let center_sub = label("", &["dim-label", "caption"]);
    for l in [&center_k, &center_big, &center_sub] {
        l.set_wrap(true);
        l.set_justify(gtk::Justification::Center);
        l.set_max_width_chars(14);
    }
    let center_inner = gtk::Box::new(gtk::Orientation::Vertical, 2);
    center_inner.set_valign(gtk::Align::Center);
    center_inner.set_vexpand(true);
    center_inner.append(&center_k);
    center_inner.append(&center_big);
    center_inner.append(&center_sub);
    let center = gtk::Box::new(gtk::Orientation::Vertical, 0);
    center.add_css_class("center-card");
    center.append(&center_inner);
    let radar = Radar::new(&center);
    radar.set_vexpand(true);
    radar.set_hexpand(true);

    let tray_icon = gtk::Image::new();
    let tray_label = gtk::Label::builder().wrap(true).xalign(0.0).build();
    let tray_chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let tray_text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    tray_text.set_hexpand(true);
    tray_text.append(&tray_label);
    tray_text.append(&tray_chips);
    let tray_btn = gtk::Button::with_label("Choose files");
    tray_btn.set_valign(gtk::Align::Center);
    let tray = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    tray.add_css_class("tray");
    tray.append(&tray_icon);
    tray.append(&tray_text);
    tray.append(&tray_btn);

    let net_pill = label("", &["pill"]);
    net_pill.set_valign(gtk::Align::Center);
    let net_row = adw::ActionRow::builder()
        .title("Network")
        .subtitle_selectable(true)
        .build();
    net_row.add_prefix(&gtk::Image::from_icon_name(
        "network-wireless-signal-good-symbolic",
    ));
    net_row.add_suffix(&net_pill);
    let bt_pill = label("READY", &["pill", "ok"]);
    bt_pill.set_valign(gtk::Align::Center);
    let bt_btn = gtk::Button::builder()
        .label("Settings")
        .valign(gtk::Align::Center)
        .tooltip_text("Open Bluetooth settings")
        .build();
    bt_btn.connect_clicked(|b| {
        let ctx = b.display().app_launch_context();
        if let Ok(app) = gio::AppInfo::create_from_commandline(
            "gnome-control-center bluetooth",
            None,
            gio::AppInfoCreateFlags::NONE,
        ) {
            let _ = app.launch(&[], Some(&ctx));
        }
    });
    let bt_row = adw::ActionRow::builder()
        .title("Bluetooth")
        .subtitle("Checking…")
        .build();
    bt_row.add_prefix(&gtk::Image::from_icon_name("bluetooth-active-symbolic"));
    bt_row.add_suffix(&bt_pill);
    bt_row.add_suffix(&bt_btn);
    bt_pill.set_visible(false);
    bt_btn.set_visible(false);
    let fw_row = adw::ActionRow::builder()
        .title("Firewall is on")
        .subtitle_selectable(true)
        .build();
    fw_row.add_prefix(&gtk::Image::from_icon_name("security-medium-symbolic"));
    let fw_done = gtk::Button::builder()
        .label("Done")
        .valign(gtk::Align::Center)
        .tooltip_text("I've allowed the port")
        .build();
    fw_row.add_suffix(&fw_done);
    fw_row.set_visible(false);
    let phone_row = adw::ActionRow::builder()
        .title("On the phone")
        .subtitle("Swipe down, tap Quick Share and choose Everyone.")
        .build();
    phone_row.add_prefix(&gtk::Image::from_icon_name("phone-symbolic"));
    let checks = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    checks.add_css_class("boxed-list");
    checks.append(&net_row);
    checks.append(&bt_row);
    checks.append(&fw_row);
    checks.append(&phone_row);

    let below = gtk::Box::new(gtk::Orientation::Vertical, 12);
    below.append(&tray);
    below.append(&checks);
    let clamp = adw::Clamp::builder()
        .maximum_size(540)
        .child(&below)
        .build();
    let stage = gtk::Box::new(gtk::Orientation::Vertical, 12);
    stage.add_css_class("stage");
    stage.set_hexpand(true);
    stage.append(&radar);
    stage.append(&clamp);

    // rail
    let feed = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    feed.add_css_class("feed");
    let placeholder = label("Files and links you share show up here.", &["dim-label"]);
    placeholder.set_wrap(true);
    placeholder.set_margin_top(12);
    placeholder.set_valign(gtk::Align::Start);
    placeholder.set_xalign(0.0);
    feed.set_placeholder(Some(&placeholder));
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&feed)
        .build();
    let hint = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    hint.add_css_class("dim-label");
    hint.append(&label("Ctrl", &["keycap"]));
    hint.append(&label("V", &["keycap"]));
    hint.append(&label("sends what you copied", &["caption"]));
    let rail_title = label("ACTIVITY", &["eyebrow"]);
    rail_title.set_xalign(0.0);
    let rail = gtk::Box::new(gtk::Orientation::Vertical, 12);
    rail.add_css_class("rail");
    rail.set_width_request(300);
    rail.append(&rail_title);
    rail.append(&scrolled);
    rail.append(&hint);

    let sep = gtk::Separator::new(gtk::Orientation::Vertical);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&stage);
    body.append(&sep);
    body.append(&rail);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let banner = adw::Banner::new("");
    view.add_top_bar(&banner);
    view.set_content(Some(&toasts));

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("QuickShare for Linux")
        .default_width(1000)
        .default_height(740)
        .width_request(360)
        .height_request(480)
        .content(&view)
        .build();
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        780.0,
        adw::LengthUnit::Sp,
    ));
    bp.add_setter(
        &body,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    bp.add_setter(
        &sep,
        "orientation",
        Some(&gtk::Orientation::Horizontal.to_value()),
    );
    bp.add_setter(&rail, "width-request", Some(&(-1).to_value()));
    bp.add_setter(&scrolled, "vexpand", Some(&false.to_value()));
    bp.add_setter(
        &scrolled,
        "propagate-natural-height",
        Some(&true.to_value()),
    );
    win.add_breakpoint(bp);

    let start_vis = prefs.start_visibility;
    let ui = Rc::new(Ui {
        app: app.clone(),
        win: win.clone(),
        toasts,
        banner: banner.clone(),
        update: RefCell::new(None),
        radar,
        center_k,
        center_big,
        center_sub,
        tray: tray.clone(),
        tray_icon,
        tray_label,
        tray_chips,
        tray_btn: tray_btn.clone(),
        checks,
        net_row,
        net_pill,
        vpn: RefCell::new(None),
        firewall: RefCell::new(None),
        fw_row,
        isolated: Cell::new(false),
        started: Instant::now(),
        bt_row,
        bt_pill,
        bt_btn,
        feed,
        vis_label,
        vis_dot,
        core,
        st: RefCell::new(State {
            prefs,
            devices: vec![],
            staged: None,
            transfers: HashMap::new(),
            pending: HashMap::new(),
            vis_until: None,
            hold: None,
        }),
    });

    // core events -> UI (the receiver is runtime-agnostic, so the GLib loop can await it)
    let u = ui.clone();
    glib::spawn_future_local(async move {
        while let Some(ev) = events.recv().await {
            u.on_event(ev);
        }
    });

    let u = ui.clone();
    tray_btn.connect_clicked(move |_| {
        let staged = u.st.borrow().staged.is_some();
        if staged {
            u.st.borrow_mut().staged = None;
            u.refresh();
        } else {
            u.choose_files(None);
        }
    });

    // drag and drop anywhere in the window
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let u = ui.clone();
    drop.connect_drop(move |_, value, _, _| {
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
        if paths.is_empty() {
            return false;
        }
        u.stage(Staged::Files(paths));
        true
    });
    let t = tray.clone();
    drop.connect_enter(move |_, _, _| {
        t.add_css_class("drop-hover");
        gdk::DragAction::COPY
    });
    let t = tray.clone();
    drop.connect_leave(move |_| t.remove_css_class("drop-hover"));
    win.add_controller(drop);

    // window actions; Ctrl+V via a local shortcut so text fields keep their own paste
    let u = ui.clone();
    let open = gio::ActionEntry::builder("open")
        .activate(move |_: &adw::ApplicationWindow, _, _| u.choose_files(None))
        .build();
    let u = ui.clone();
    let paste = gio::ActionEntry::builder("paste")
        .activate(move |_: &adw::ApplicationWindow, _, _| u.paste())
        .build();
    win.add_action_entries([open, paste]);
    let shortcuts = gtk::ShortcutController::new();
    for (accel, action) in [("<Control>o", "win.open"), ("<Control>v", "win.paste")] {
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(accel),
            Some(gtk::NamedAction::new(action)),
        ));
    }
    win.add_controller(shortcuts);

    // app actions
    let u = ui.clone();
    let visibility = gio::ActionEntry::builder("visibility")
        .parameter_type(Some(&String::static_variant_type()))
        .state(start_vis.key().to_variant())
        .activate(move |_: &adw::Application, action, param| {
            let Some(v) = param.and_then(|p| p.get::<String>()) else {
                return;
            };
            action.set_state(&v.to_variant());
            u.apply_visibility(Visibility::from_key(&v));
        })
        .build();
    let u = ui.clone();
    let preferences = gio::ActionEntry::builder("preferences")
        .activate(move |_: &adw::Application, _, _| dialogs::preferences(&u))
        .build();
    let u = ui.clone();
    let about = gio::ActionEntry::builder("about")
        .activate(move |_: &adw::Application, _, _| {
            adw::AboutDialog::builder()
                .application_name("QuickShare for Linux")
                .application_icon("send-to-symbolic")
                .developer_name("mhfmobin")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Share files and links with nearby Android phones using Quick Share.")
                .website(env!("CARGO_PKG_REPOSITORY"))
                .issue_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/issues"))
                .license_type(gtk::License::Gpl30)
                .build()
                .present(Some(&u.win));
        })
        .build();
    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build();
    let u = ui.clone();
    let accept = gio::ActionEntry::builder("accept")
        .parameter_type(Some(&u64::static_variant_type()))
        .activate(move |_: &adw::Application, _, p| {
            if let Some(id) = p.and_then(|p| p.get::<u64>()) {
                u.answer(id, true, false);
            }
        })
        .build();
    let u = ui.clone();
    let decline = gio::ActionEntry::builder("decline")
        .parameter_type(Some(&u64::static_variant_type()))
        .activate(move |_: &adw::Application, _, p| {
            if let Some(id) = p.and_then(|p| p.get::<u64>()) {
                u.answer(id, false, false);
            }
        })
        .build();
    let u = ui.clone();
    let show_incoming = gio::ActionEntry::builder("show-incoming")
        .parameter_type(Some(&u64::static_variant_type()))
        .activate(move |_: &adw::Application, _, p| {
            if let Some(id) = p.and_then(|p| p.get::<u64>()) {
                u.show_pending(id);
            }
        })
        .build();
    let u = ui.clone();
    let show_window = gio::ActionEntry::builder("show-window")
        .activate(move |_: &adw::Application, _, _| u.win.present())
        .build();
    app.add_action_entries([
        visibility,
        preferences,
        about,
        quit,
        accept,
        decline,
        show_incoming,
        show_window,
    ]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("app.preferences", &["<Control>comma"]);

    // closing keeps the app alive in the background (if enabled)
    let u = ui.clone();
    win.connect_close_request(move |w| {
        if u.st.borrow().prefs.background {
            w.set_visible(false);
            u.hold();
            glib::Propagation::Stop
        } else {
            u.st.borrow_mut().hold = None;
            glib::Propagation::Proceed
        }
    });

    // wake phones over Bluetooth only while the window is open (that's when you'd send)
    let u = ui.clone();
    win.connect_visible_notify(move |w| u.core.set_wake_phones(w.is_visible()));
    ui.apply_visibility(start_vis);
    ui.refresh();
    let u = ui.clone();
    fw_done.connect_clicked(move |_| {
        {
            let mut st = u.st.borrow_mut();
            st.prefs.firewall_ok_port = u.core.port();
            st.prefs.save();
        }
        u.refresh();
    });
    if ui.core.port() != ui.st.borrow().prefs.port {
        ui.toast(&format!(
            "Port {} is in use, so port {} is used this time",
            ui.st.borrow().prefs.port,
            ui.core.port()
        ));
    }
    check_network(&ui);
    let weak = Rc::downgrade(&ui);
    glib::timeout_add_seconds_local(5, move || match weak.upgrade() {
        Some(u) => {
            if u.win.is_visible() {
                check_network(&u);
            }
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    if update::enabled() {
        let u = ui.clone();
        banner.connect_button_clicked(move |_| update::on_button(&u));
        update::check(&ui);
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_seconds_local(6 * 3600, move || match weak.upgrade() {
            Some(u) => {
                update::check(&u);
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }
    Ok(ui)
}

/// Output of a short command, or None if it can't run.
async fn output(argv: &[&str]) -> Option<String> {
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let proc = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;
    proc.communicate_utf8_future(None)
        .await
        .ok()?
        .0
        .map(|s| s.to_string())
}

/// A VPN tunnel interface that is up, by name. Its LAN kill switch can hide nearby devices.
/// ponytail: name-based; tunnels with other names go unnoticed.
fn vpn_interface() -> Option<String> {
    std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| {
            [
                "tun", "wg", "nordlynx", "nordtun", "proton", "mullvad", "ppp",
            ]
            .iter()
            .any(|p| n.starts_with(p))
        })
}

/// Network problems for the checks list: VPN, firewall, isolated Wi-Fi. Runs every 5 s while the window is open.
fn check_network(ui: &Rc<Ui>) {
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let port = ui.core.port();
        let vpn = match vpn_interface() {
            // NordVPN's firewall drops all LAN traffic unless LAN discovery is on.
            Some(n) if n.starts_with("nord") => output(&["nordvpn", "settings"])
                .await
                .filter(|out| out.lines().any(|l| l.trim() == "LAN Discovery: disabled"))
                .map(|_| "NordVPN is blocking nearby devices. Run: nordvpn set lan-discovery on".to_string()),
            Some(n) => Some(format!(
                "A VPN is on ({n}). If phones don't show up, allow local network access in the VPN, or disconnect it while sharing."
            )),
            None => None,
        };
        let ufw = std::fs::read_to_string("/etc/ufw/ufw.conf")
            .is_ok_and(|c| c.lines().any(|l| l.trim() == "ENABLED=yes"));
        let firewall = if ufw {
            Some(format!(
                "sudo ufw allow {port}/tcp && sudo ufw allow 5353/udp"
            ))
        } else if output(&["systemctl", "is-active", "firewalld"])
            .await
            .as_deref()
            .map(str::trim)
            == Some("active")
        {
            Some(format!(
                "sudo firewall-cmd --permanent --add-port={port}/tcp --add-service=mdns && sudo firewall-cmd --reload"
            ))
        } else {
            None
        };
        let isolated = gio::NetworkMonitor::default().is_network_available()
            && !ui.core.lan_active()
            && ui.started.elapsed() > Duration::from_secs(45);
        let changed = *ui.vpn.borrow() != vpn
            || *ui.firewall.borrow() != firewall
            || ui.isolated.get() != isolated;
        if changed {
            *ui.vpn.borrow_mut() = vpn;
            *ui.firewall.borrow_mut() = firewall;
            ui.isolated.set(isolated);
            ui.refresh();
        }
    });
}

/// Bundled fonts (OFL): installed next to the binary's share dir, or the source tree when run uninstalled.
fn load_fonts() {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join("share/quickshare-linux/fonts")));
    let source = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../data/fonts"));
    let Some(dir) = installed.into_iter().chain([source]).find(|d| d.is_dir()) else {
        return;
    };
    let map = pangocairo::FontMap::default();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path().extension().is_some_and(|e| e == "ttf") {
            let _ = map.add_font_file(entry.path());
        }
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.add_main_option(
        "background",
        glib::Char::from(b'b'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start without opening the window",
        None,
    );
    app.connect_handle_local_options(|_, opts| {
        if opts.contains("background") {
            START_HIDDEN.with(|c| c.set(true));
        }
        -1
    });

    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();
    let u = ui.clone();
    app.connect_startup(move |app| {
        load_fonts();
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        match build(app) {
            Ok(built) => *u.borrow_mut() = Some(built),
            Err(e) => {
                let page = adw::StatusPage::builder()
                    .icon_name("dialog-error-symbolic")
                    .title("Sharing couldn't start")
                    .description(format!("{e}\n\nAnother copy may already be running, or the network is unavailable."))
                    .build();
                let view = adw::ToolbarView::new();
                view.add_top_bar(&adw::HeaderBar::new());
                view.set_content(Some(&page));
                let _win = adw::ApplicationWindow::builder().application(app).title("QuickShare for Linux").default_width(480).default_height(400).content(&view).build();
            }
        }
    });
    let u = ui.clone();
    app.connect_activate(move |app| match u.borrow().as_ref() {
        Some(ui) if START_HIDDEN.with(|c| c.replace(false)) => ui.hold(),
        Some(ui) => ui.win.present(),
        None => {
            if let Some(w) = app.windows().first() {
                w.present();
            }
        }
    });
    let u = ui.clone();
    app.connect_shutdown(move |_| {
        if let Some(ui) = u.borrow().as_ref() {
            ui.core.shutdown();
        }
    });
    // Ctrl+C / pkill quit cleanly too, so the shutdown above runs
    for signal in [2, 15] {
        let app = app.clone();
        glib::unix_signal_add_local(signal, move || {
            app.quit();
            glib::ControlFlow::Break
        });
    }
    let u = ui.clone();
    app.connect_open(move |_, files, _| {
        let Some(ui) = u.borrow().clone() else { return };
        let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
        if !paths.is_empty() {
            ui.stage(Staged::Files(paths));
        }
        ui.win.present();
    });
    let code = app.run();
    if RESTART.with(Cell::get) {
        use std::os::unix::process::CommandExt;
        let e = std::process::Command::new(update::EXE).exec();
        eprintln!("qs: restart failed: {e}");
    }
    code
}

/// Whether `dir` lists an adapter (with `marker` inside its folder, if given).
fn has_adapter(dir: &str, marker: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.path().join(marker).exists())))
}
