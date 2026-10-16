//! mDNS: advertise ourselves (receiving) and browse for visible phones (sending).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::transfer::{endpoint_info, parse_endpoint_info};
use crate::{Device, Event, Inner, Route};

const TY: &str = "_FC9F5ED42C8A._tcp.local.";
pub(crate) const SERVICE_ID: [u8; 3] = [0xFC, 0x9F, 0x5E]; // SHA256("NearbySharing")[..3]

pub struct Mdns {
    daemon: ServiceDaemon,
    inner: Arc<Inner>,
    port: u16,
    registered: Mutex<Option<String>>,
    /// Another device answered over mDNS, so this network passes multicast between devices.
    pub lan_active: Arc<AtomicBool>,
}

fn instance_name(endpoint_id: &[u8; 4]) -> String {
    let mut b = vec![0x23];
    b.extend_from_slice(endpoint_id);
    b.extend_from_slice(&SERVICE_ID);
    b.extend_from_slice(&[0, 0]);
    B64.encode(b)
}

/// Physical NICs (Wi-Fi, Ethernet) have a `device` link in sysfs; Docker bridges, veths and VPN tunnels don't.
/// Phones can't reach those, so advertising them makes connections fail.
fn physical_interfaces() -> Vec<String> {
    std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("device").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn parse(info: &ServiceInfo, own_id: &[u8; 4]) -> Option<Device> {
    let instance = info.get_fullname().split('.').next()?;
    let raw = B64.decode(instance.trim_end_matches('=')).ok()?;
    if raw.len() < 10 || raw[0] != 0x23 || raw[5..8] != SERVICE_ID || raw[1..5] == own_id[..] {
        return None;
    }
    let n = info.get_property_val_str("n")?;
    let (name, kind) = parse_endpoint_info(&B64.decode(n.trim_end_matches('=')).ok()?)?;
    let addrs = info.get_addresses();
    let ip = addrs.iter().find(|a| a.is_ipv4()).or(addrs.iter().next())?;
    Some(Device {
        id: String::from_utf8_lossy(&raw[1..5]).into(),
        name,
        kind,
        route: Route::Lan((*ip, info.get_port()).into()),
    })
}

impl Mdns {
    pub fn start(inner: Arc<Inner>, port: u16) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let nics = physical_interfaces();
        eprintln!("qs: mdns on {nics:?}");
        if !nics.is_empty() {
            // ponytail: IPv4 on physical NICs only; Android reliably dials IPv4 LAN addresses
            daemon.disable_interface(IfKind::All)?;
            for nic in nics {
                daemon.enable_interface(IfKind::Name(nic))?;
            }
            daemon.disable_interface(IfKind::IPv6)?;
        }
        let lan_active = Arc::new(AtomicBool::new(false));
        // Any other service type answering also counts (guest Wi-Fi usually blocks this).
        // Only other devices answer for other types: ours is filtered out.
        if let Ok(meta) = daemon.browse("_services._dns-sd._udp.local.") {
            let active = lan_active.clone();
            std::thread::spawn(move || {
                while let Ok(ev) = meta.recv() {
                    if let ServiceEvent::ServiceFound(_, name) = ev {
                        if !name.contains("_FC9F5ED42C8A") {
                            active.store(true, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
        let rx = daemon.browse(TY)?;
        let events = inner.clone();
        let active = lan_active.clone();
        std::thread::spawn(move || {
            let mut seen: HashMap<String, String> = HashMap::new(); // fullname -> endpoint id
            while let Ok(ev) = rx.recv() {
                match ev {
                    ServiceEvent::ServiceResolved(info) => {
                        eprintln!(
                            "qs: mdns saw {} at {:?}:{}",
                            info.get_fullname(),
                            info.get_addresses(),
                            info.get_port()
                        );
                        if let Some(dev) = parse(&info, &events.endpoint_id) {
                            active.store(true, Ordering::Relaxed);
                            seen.insert(info.get_fullname().into(), dev.id.clone());
                            events.emit(Event::DeviceFound(dev));
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, full) => {
                        if let Some(id) = seen.remove(&full) {
                            events.emit(Event::DeviceLost(id));
                        }
                    }
                    _ => {}
                }
            }
        });
        let m = Mdns {
            daemon,
            inner,
            port,
            registered: Mutex::new(None),
            lan_active,
        };
        m.set_visible(true);
        Ok(m)
    }

    pub fn set_visible(&self, visible: bool) {
        let mut reg = self.registered.lock().unwrap();
        match (visible, reg.is_some()) {
            (true, true) | (false, false) => {}
            (false, true) => {
                let _ = self.daemon.unregister(&reg.take().unwrap());
            }
            (true, false) => *reg = self.register(),
        }
    }

    /// Re-announce with the current name. Registering the same instance again updates it in place;
    /// never unregister first, or the goodbye packet can race the new announcement and hide us.
    pub fn readvertise(&self) {
        let mut reg = self.registered.lock().unwrap();
        if reg.is_some() {
            *reg = self.register();
        }
    }

    fn register(&self) -> Option<String> {
        let id = &self.inner.endpoint_id;
        let name = self.inner.settings.lock().unwrap().name.clone();
        let txt = B64.encode(endpoint_info(&name));
        let host = format!("qs-linux-{}.local.", String::from_utf8_lossy(id));
        let info = ServiceInfo::new(
            TY,
            &instance_name(id),
            &host,
            "",
            self.port,
            &[("n", txt.as_str())][..],
        )
        .ok()?
        .enable_addr_auto();
        let full = info.get_fullname().to_string();
        match self.daemon.register(info) {
            Ok(()) => Some(full),
            Err(e) => {
                eprintln!("qs: mdns register failed: {e}");
                None
            }
        }
    }
}
