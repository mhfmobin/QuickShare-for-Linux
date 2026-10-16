use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use qs_core::{Medium, TextKind, TransferId};

use crate::prefs::Visibility;
use crate::{describe_incoming, is_url, mime_icon, Ui};

fn dim(text: &str) -> gtk::Label {
    let l = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .build();
    l.add_css_class("dim-label");
    l
}

/// Accept / Decline an incoming share, showing the PIN to compare with the phone.
pub fn incoming(ui: &Rc<Ui>, id: TransferId) {
    let st = ui.st.borrow();
    let Some((req, _)) = st.pending.get(&id) else {
        return;
    };
    let what = if req.texts.is_empty() && req.files.len() == 1 {
        "a file".to_string()
    } else {
        describe_incoming(req)
    };
    let heading = format!("{} wants to share {what}", req.from);
    let dlg = adw::AlertDialog::new(Some(&heading), None);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);

    let pin_hint = dim("Make sure this PIN matches the one on the phone.");
    pin_hint.set_hexpand(true);
    pin_hint.add_css_class("caption");
    let pin = gtk::Label::new(Some(&req.pin));
    pin.add_css_class("pin");
    let pinrow = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    pinrow.add_css_class("pinrow");
    pinrow.append(&pin_hint);
    pinrow.append(&pin);
    body.append(&pinrow);

    for (title, kind) in &req.texts {
        let preview = gtk::Label::builder()
            .label(title)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        preview.add_css_class("urlbox");
        if *kind != TextKind::Url {
            preview.set_tooltip_text(Some("Preview. The full text arrives after you accept."));
        }
        body.append(&preview);
    }
    if !req.files.is_empty() {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        for f in req.files.iter().take(5) {
            let size = glib::format_size(f.size);
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&f.name))
                .subtitle(if f.folder.is_empty() {
                    size.to_string()
                } else {
                    format!("{} · {size}", glib::markup_escape_text(&f.folder.join("/")))
                })
                .title_lines(1)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(mime_icon(&f.mime)));
            list.append(&row);
        }
        if req.files.len() > 5 {
            let total: u64 = req.files.iter().map(|f| f.size).sum();
            list.append(
                &adw::ActionRow::builder()
                    .title(format!("and {} more", req.files.len() - 5))
                    .subtitle(format!("{} in total", glib::format_size(total)))
                    .build(),
            );
        }
        body.append(&list);
    }

    if req.medium == Medium::Bluetooth {
        let note = dim(
            "Connected over Bluetooth, which is slow. If Wi‑Fi is on, this PC briefly joins the phone's Wi‑Fi after you accept to go faster, then reconnects to your network.",
        );
        note.add_css_class("caption");
        body.append(&note);
    }

    let trust = gtk::CheckButton::with_label(&format!("Accept from {} without asking", req.from));
    let warn = dim(&format!(
        "Any phone can call itself {}, so only turn this on for networks you trust.",
        req.from
    ));
    warn.add_css_class("caption");
    warn.set_margin_start(28);
    body.append(&trust);
    body.append(&warn);

    dlg.set_extra_child(Some(&body));
    dlg.add_responses(&[("decline", "_Decline"), ("accept", "_Accept")]);
    dlg.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("accept"));
    dlg.set_close_response("decline");
    drop(st);

    let u = ui.clone();
    dlg.connect_response(None, move |_, r| {
        u.answer(id, r == "accept", trust.is_active())
    });
    ui.attach_dialog(id, dlg.clone());
    dlg.present(Some(&ui.win));
}

/// The transfer crawls over Bluetooth because Wi-Fi is off: explain, and offer to turn it on.
/// `row_btn` is the transfer row's "Turn on Wi‑Fi…" button, hidden once Wi-Fi is on its way.
pub fn wifi_off(ui: &Rc<Ui>, row_btn: &gtk::Button) {
    let dlg = adw::AlertDialog::new(
        Some("Speed Up With Wi‑Fi?"),
        Some(
            "Wi‑Fi is off on this computer, so this transfer is going over Bluetooth. \
             Bluetooth is slow: about 20–50 KB a second, so a 100 MB video takes around an hour.",
        ),
    );
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let icon = gtk::Image::from_icon_name("network-wireless-symbolic");
    icon.set_pixel_size(48);
    icon.add_css_class("accent");
    body.append(&icon);
    body.append(&dim(
        "Turn on Wi‑Fi and the transfer moves to a direct Wi‑Fi link with the phone, \
         usually hundreds of times faster. You don't need to be on any network for this.",
    ));
    dlg.set_extra_child(Some(&body));
    dlg.add_responses(&[("keep", "Keep _Bluetooth"), ("wifi", "_Turn On Wi‑Fi")]);
    dlg.set_response_appearance("wifi", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("wifi"));
    dlg.set_close_response("keep");
    let (u, b) = (ui.clone(), row_btn.clone());
    dlg.connect_response(Some("wifi"), move |_, _| {
        b.set_visible(false);
        u.core.turn_on_wifi();
        u.toast("Turning on Wi‑Fi…");
    });
    dlg.present(Some(&ui.win));
}

/// A received link or text: copy it, or open the link.
pub fn text(ui: &Rc<Ui>, from: &str, text: &str) {
    let url = is_url(text);
    let dlg = adw::AlertDialog::new(
        Some(&format!(
            "{} from {from}",
            if url { "Link" } else { "Text" }
        )),
        None,
    );
    let content = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .build();
    content.add_css_class("urlbox");
    dlg.set_extra_child(Some(&content));
    dlg.add_responses(&[("close", "_Close"), ("copy", "_Copy")]);
    if url {
        dlg.add_response("open", "_Open in Browser");
        dlg.set_response_appearance("open", adw::ResponseAppearance::Suggested);
        dlg.set_default_response(Some("open"));
    }
    dlg.set_close_response("close");
    let (u, t) = (ui.clone(), text.to_string());
    dlg.connect_response(None, move |_, r| match r {
        "copy" => {
            u.win.clipboard().set_text(&t);
            u.toast("Copied");
        }
        "open" => gtk::UriLauncher::new(&t).launch(Some(&u.win), None::<&gio::Cancellable>, |_| {}),
        _ => {}
    });
    dlg.present(Some(&ui.win));
}

pub fn preferences(ui: &Rc<Ui>) {
    let p = ui.st.borrow().prefs.clone();
    let page = adw::PreferencesPage::new();

    // this device
    let device = adw::PreferencesGroup::builder()
        .title("This device")
        .description("Phones show this name in their share sheet.")
        .build();
    let name = adw::EntryRow::builder()
        .title("Name")
        .text(&p.name)
        .show_apply_button(true)
        .build();
    let u = ui.clone();
    name.connect_apply(move |row| {
        let n = row.text().trim().to_string();
        if n.is_empty() {
            row.set_text(&u.st.borrow().prefs.name);
            return;
        }
        u.core.set_name(n.clone());
        let mut st = u.st.borrow_mut();
        st.prefs.name = n;
        st.prefs.save();
    });
    device.add(&name);
    page.add(&device);

    // visibility on start
    let vis = adw::PreferencesGroup::builder()
        .title("Visibility when the app starts")
        .build();
    let mut first: Option<gtk::CheckButton> = None;
    for (v, title, sub) in [
        (
            Visibility::Everyone,
            "Everyone",
            "Any nearby phone can see this PC",
        ),
        (
            Visibility::Temporary,
            "Everyone for 10 minutes",
            "Then switches to Hidden",
        ),
        (Visibility::Hidden, "Hidden", "You can still send to phones"),
    ] {
        let check = gtk::CheckButton::builder()
            .active(p.start_visibility == v)
            .valign(gtk::Align::Center)
            .build();
        if let Some(f) = &first {
            check.set_group(Some(f));
        } else {
            first = Some(check.clone());
        }
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(sub)
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        let u = ui.clone();
        check.connect_toggled(move |c| {
            if c.is_active() {
                let mut st = u.st.borrow_mut();
                st.prefs.start_visibility = v;
                st.prefs.save();
            }
        });
        vis.add(&row);
    }
    page.add(&vis);

    // receiving
    let recv = adw::PreferencesGroup::builder().title("Receiving").build();
    let folder = adw::ActionRow::builder()
        .title("Save files to")
        .subtitle(p.download_dir.to_string_lossy())
        .build();
    folder.add_css_class("property");
    let change = gtk::Button::builder()
        .label("Change")
        .valign(gtk::Align::Center)
        .build();
    folder.add_suffix(&change);
    let (u, row) = (ui.clone(), folder.clone());
    change.connect_clicked(move |_| {
        let (u, row) = (u.clone(), row.clone());
        gtk::FileDialog::builder()
            .title("Save received files to")
            .build()
            .select_folder(
                Some(&u.win.clone()),
                None::<&gio::Cancellable>,
                move |res| {
                    let Some(path) = res.ok().and_then(|f| f.path()) else {
                        return;
                    };
                    u.core.set_download_dir(path.clone());
                    row.set_subtitle(&path.to_string_lossy());
                    let mut st = u.st.borrow_mut();
                    st.prefs.download_dir = path;
                    st.prefs.save();
                },
            );
    });
    recv.add(&folder);
    if p.trusted.is_empty() {
        recv.add(
            &adw::ActionRow::builder()
                .title("No trusted devices")
                .subtitle("Tick “Accept without asking” when a phone shares with you.")
                .build(),
        );
    }
    for trusted in &p.trusted {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(trusted))
            .subtitle("Accepted without asking")
            .build();
        let remove = gtk::Button::builder()
            .label("Remove")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        remove.add_css_class("error");
        row.add_suffix(&remove);
        let (u, g, r, name) = (ui.clone(), recv.clone(), row.clone(), trusted.clone());
        remove.connect_clicked(move |_| {
            let mut st = u.st.borrow_mut();
            st.prefs.trusted.retain(|t| *t != name);
            st.prefs.save();
            g.remove(&r);
        });
        recv.add(&row);
    }
    page.add(&recv);

    // network
    let net = adw::PreferencesGroup::builder()
        .title("Network")
        .description(format!(
            "Phones connect to this port; allow it in your firewall. In use now: {}.",
            ui.core.port()
        ))
        .build();
    let port = adw::SpinRow::with_range(1024.0, 65535.0, 1.0);
    port.set_title("Port");
    port.set_subtitle("Takes effect after restarting the app");
    port.set_value(p.port.into());
    let u = ui.clone();
    port.connect_value_notify(move |r| {
        let mut st = u.st.borrow_mut();
        st.prefs.port = r.value() as u16;
        st.prefs.save();
    });
    net.add(&port);
    page.add(&net);

    // background
    let bg = adw::PreferencesGroup::builder().title("Background").build();
    let keep = adw::SwitchRow::builder()
        .title("Keep running when the window is closed")
        .subtitle("Stay visible and show shares as notifications")
        .active(p.background)
        .build();
    let u = ui.clone();
    keep.connect_active_notify(move |r| {
        let mut st = u.st.borrow_mut();
        st.prefs.background = r.is_active();
        if !r.is_active() {
            st.hold = None;
        }
        st.prefs.save();
    });
    let login = adw::SwitchRow::builder()
        .title("Start at login")
        .subtitle("Opens hidden, ready to receive")
        .active(p.autostart)
        .build();
    let u = ui.clone();
    login.connect_active_notify(move |r| {
        let mut st = u.st.borrow_mut();
        st.prefs.set_autostart(r.is_active());
        st.prefs.save();
    });
    bg.add(&keep);
    bg.add(&login);
    page.add(&bg);

    let dlg = adw::PreferencesDialog::new();
    dlg.add(&page);
    dlg.present(Some(&ui.win));
}
