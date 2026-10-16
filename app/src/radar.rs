//! The radar: this PC in the middle, nearby devices placed on a ring around it.
//! Rings and the scanning sweep are drawn; the center card and device nodes are real child widgets.

use std::f64::consts::PI;

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

/// Fixed slots so devices never overlap and don't jump around: (angle in degrees, radius as fraction of size).
const SLOTS: [(f32, f32); 10] = [
    (-40.0, 0.39),
    (200.0, 0.39),
    (65.0, 0.39),
    (-135.0, 0.39),
    (125.0, 0.39),
    (10.0, 0.39),
    (-90.0, 0.39),
    (165.0, 0.39),
    (95.0, 0.39),
    (-170.0, 0.39),
];

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct Radar {
        pub center: RefCell<Option<gtk::Widget>>,
        pub nodes: RefCell<Vec<(gtk::Widget, usize)>>,
        pub sweep: Cell<f64>,
        pub scanning: Cell<bool>,
        pub anim: RefCell<Option<adw::TimedAnimation>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Radar {
        const NAME: &'static str = "QsRadar";
        type Type = super::Radar;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("radar");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }
    }

    impl ObjectImpl for Radar {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Radar {
        fn measure(&self, _o: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (300, 480, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let s = width.min(height) as f32;
            let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
            let at =
                |x: f32, y: f32| Some(gsk::Transform::new().translate(&graphene::Point::new(x, y)));
            if let Some(c) = &*self.center.borrow() {
                let d = (s * 0.42) as i32;
                c.allocate(d, d, -1, at(cx - d as f32 / 2.0, cy - d as f32 / 2.0));
            }
            for (node, slot) in self.nodes.borrow().iter() {
                let (angle, r) = SLOTS[slot % SLOTS.len()];
                let r = if *slot >= SLOTS.len() { r * 0.78 } else { r };
                let (_, nat) = node.preferred_size();
                let (x, y) = (
                    cx + angle.to_radians().cos() * r * s,
                    cy + angle.to_radians().sin() * r * s,
                );
                // anchor on the disc's center (disc is 58px + 4px padding), not the whole node
                node.allocate(
                    nat.width(),
                    nat.height(),
                    -1,
                    at(x - nat.width() as f32 / 2.0, y - 33.0),
                );
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (w, h) = (obj.width() as f64, obj.height() as f64);
            let s = w.min(h);
            let cr = snapshot.append_cairo(&graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
            let line = obj.color();
            cr.set_source_rgba(
                line.red() as f64,
                line.green() as f64,
                line.blue() as f64,
                line.alpha() as f64 * 0.18,
            );
            cr.set_line_width(1.0);
            cr.set_dash(&[4.0, 4.0], 0.0);
            for r in [0.495, 0.375, 0.255] {
                cr.arc(w / 2.0, h / 2.0, s * r, 0.0, 2.0 * PI);
                let _ = cr.stroke();
            }
            if self.scanning.get() {
                let accent = adw::StyleManager::default().accent_color_rgba();
                cr.set_dash(&[], 0.0);
                for offset in [0.0, 0.5] {
                    let p = (self.sweep.get() + offset) % 1.0;
                    cr.set_source_rgba(
                        accent.red() as f64,
                        accent.green() as f64,
                        accent.blue() as f64,
                        0.5 * (1.0 - p),
                    );
                    cr.arc(w / 2.0, h / 2.0, s * 0.19 * (1.0 + 1.6 * p), 0.0, 2.0 * PI);
                    let _ = cr.stroke();
                }
            }
            drop(cr);
            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct Radar(ObjectSubclass<imp::Radar>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Radar {
    pub fn new(center: &impl IsA<gtk::Widget>) -> Self {
        let radar: Self = glib::Object::new();
        center.set_parent(&radar);
        radar.imp().center.replace(Some(center.clone().upcast()));
        let weak = radar.downgrade();
        let target = adw::CallbackAnimationTarget::new(move |v| {
            if let Some(r) = weak.upgrade() {
                r.imp().sweep.set(v);
                r.queue_draw();
            }
        });
        let anim = adw::TimedAnimation::builder()
            .widget(&radar)
            .value_from(0.0)
            .value_to(1.0)
            .duration(3600)
            .repeat_count(0)
            .easing(adw::Easing::Linear)
            .target(&target)
            .build();
        radar.imp().anim.replace(Some(anim));
        radar.set_scanning(true);
        radar
    }

    /// Scanning shows the expanding sweep; turned off while a transfer runs.
    pub fn set_scanning(&self, on: bool) {
        let imp = self.imp();
        if imp.scanning.replace(on) == on {
            return;
        }
        if let Some(a) = &*imp.anim.borrow() {
            if on {
                a.play()
            } else {
                a.pause()
            }
        }
        self.queue_draw();
    }

    /// Adds a node in the first free slot.
    pub fn add_node(&self, node: &impl IsA<gtk::Widget>) {
        let mut nodes = self.imp().nodes.borrow_mut();
        let slot = (0..)
            .find(|s| !nodes.iter().any(|(_, used)| used == s))
            .unwrap();
        node.set_parent(self);
        nodes.push((node.clone().upcast(), slot));
        drop(nodes);
        self.queue_allocate();
    }

    pub fn remove_node(&self, node: &impl IsA<gtk::Widget>) {
        let node = node.as_ref();
        self.imp().nodes.borrow_mut().retain(|(n, _)| n != node);
        node.unparent();
        self.queue_allocate();
    }
}
