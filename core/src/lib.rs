//! Quick Share (Nearby Share) protocol over Wi-Fi LAN. No GTK in here.

mod ble;
mod bt;
mod chan;
mod crypto;
mod hotspot;
mod mdns;
mod transfer;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;

#[allow(clippy::all, dead_code)]
pub(crate) mod proto {
    pub mod securegcm {
        include!(concat!(env!("OUT_DIR"), "/securegcm.rs"));
    }
    pub mod securemessage {
        include!(concat!(env!("OUT_DIR"), "/securemessage.rs"));
    }
    pub mod location {
        pub mod nearby {
            pub mod connections {
                include!(concat!(env!("OUT_DIR"), "/location.nearby.connections.rs"));
            }
            pub mod proto {
                pub mod sharing {
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/location.nearby.proto.sharing.rs"
                    ));
                }
            }
        }
    }
    pub mod sharing {
        pub mod nearby {
            include!(concat!(env!("OUT_DIR"), "/sharing.nearby.rs"));
        }
    }
    pub use self::sharing::nearby as share;
    pub use location::nearby::connections;
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Decode(prost::DecodeError),
    Protocol(&'static str),
    Closed,
    Cancelled,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<prost::DecodeError> for Error {
    fn from(e: prost::DecodeError) -> Self {
        Error::Decode(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Decode(e) => write!(f, "invalid data: {e}"),
            Error::Protocol(m) => write!(f, "{m}"),
            Error::Closed => write!(f, "connection lost"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

pub type TransferId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Unknown = 0,
    Phone = 1,
    Tablet = 2,
    Laptop = 3,
}

/// How to reach a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Lan(SocketAddr),
    /// Classic Bluetooth address (no shared network needed)
    Bluetooth([u8; 6]),
}

/// What a transfer currently runs over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Lan,
    /// Slow (roughly 0.1–0.3 MB/s)
    Bluetooth,
    /// The phone's own Wi-Fi hotspot, joined for this transfer
    Direct,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub route: Route,
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    /// Folder path inside the share (empty at the top level), already made safe.
    pub folder: Vec<String>,
    pub size: u64,
    pub mime: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Url,
    Address,
    Phone,
}

impl From<proto::share::text_metadata::Type> for TextKind {
    fn from(t: proto::share::text_metadata::Type) -> Self {
        use proto::share::text_metadata::Type::*;
        match t {
            Url => TextKind::Url,
            Address => TextKind::Address,
            PhoneNumber => TextKind::Phone,
            _ => TextKind::Text,
        }
    }
}

/// A Wi-Fi network shared from the phone's Wi-Fi settings.
#[derive(Debug, Clone)]
pub struct WifiNetwork {
    pub ssid: String,
    pub password: String,
    pub security: WifiSecurity,
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiSecurity {
    Open,
    Wpa,
    Wep,
    /// WPA3
    Sae,
}

/// What to send: files and folders (sent with their structure), and texts or links.
#[derive(Default)]
pub struct Payload {
    pub files: Vec<PathBuf>,
    pub texts: Vec<String>,
}

/// An incoming share waiting for the user. Call `answer` exactly once.
#[derive(Debug)]
pub struct Incoming {
    pub id: TransferId,
    pub from: String,
    pub kind: DeviceKind,
    pub pin: String,
    /// Bluetooth means accepting will briefly move this PC to the phone's Wi-Fi.
    pub medium: Medium,
    pub files: Vec<FileInfo>,
    /// (title, kind) of each text or link in the share
    pub texts: Vec<(String, TextKind)>,
    /// Name of each Wi-Fi network in the share
    pub wifi: Vec<String>,
    reply: oneshot::Sender<bool>,
}

impl Incoming {
    pub fn answer(self, accept: bool) {
        let _ = self.reply.send(accept);
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Received {
        /// Top-level files and folders saved in the download folder
        files: Vec<PathBuf>,
        texts: Vec<String>,
        wifi: Vec<WifiNetwork>,
    },
    Sent,
    Rejected,
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub enum Event {
    DeviceFound(Device),
    DeviceLost(String),
    Incoming(Incoming),
    /// Outgoing: secure channel up, PIN to show while the phone asks its user.
    Connected {
        id: TransferId,
        pin: String,
    },
    Accepted {
        id: TransferId,
    },
    /// The transfer runs over this now (sent at the start and on every switch).
    Medium {
        id: TransferId,
        medium: Medium,
    },
    Progress {
        id: TransferId,
        done: u64,
        total: u64,
    },
    Done {
        id: TransferId,
        outcome: Outcome,
    },
    /// Bluetooth adapter present and powered on.
    Bluetooth(bool),
    /// This transfer runs over Bluetooth because the laptop's Wi-Fi is switched off;
    /// `Handle::turn_on_wifi` lets it move to the phone's faster Wi-Fi.
    WifiOff {
        id: TransferId,
    },
}

pub struct Settings {
    pub name: String,
    pub download_dir: PathBuf,
    /// TCP port to listen on; a random one is used if it's taken (or 0).
    pub port: u16,
}

pub(crate) struct Inner {
    endpoint_id: [u8; 4],
    settings: Mutex<Settings>,
    events: mpsc::UnboundedSender<Event>,
    next_id: AtomicU64,
    tasks: Mutex<HashMap<TransferId, AbortHandle>>,
    /// True while any transfer runs. Bluetooth LE adverts and scans pause then: this laptop's
    /// Intel 9560 firmware crashes (HCI hardware error 0x0c) when they churn during pairing or a transfer.
    busy: watch::Sender<bool>,
    /// Ticks when the user turned Wi-Fi on, so Bluetooth transfers ask for the switch.
    wifi_on: watch::Sender<u32>,
}

impl Inner {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn task_done(&self, id: TransferId) -> Option<AbortHandle> {
        let mut tasks = self.tasks.lock().unwrap();
        let task = tasks.remove(&id);
        self.busy.send_replace(!tasks.is_empty());
        task
    }

    fn new(settings: Settings) -> (Arc<Self>, mpsc::UnboundedReceiver<Event>) {
        const ALNUM: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let (events, rx) = mpsc::unbounded_channel();
        let inner = Inner {
            endpoint_id: std::array::from_fn(|_| ALNUM[rand::random::<usize>() % ALNUM.len()]),
            settings: Mutex::new(settings),
            events,
            next_id: AtomicU64::new(1),
            tasks: Mutex::new(HashMap::new()),
            busy: watch::channel(false).0,
            wifi_on: watch::channel(0).0,
        };
        (Arc::new(inner), rx)
    }

    /// Run a transfer task; it always ends with exactly one `Done` event.
    fn spawn<F>(
        self: &Arc<Self>,
        rt: &tokio::runtime::Handle,
        f: impl FnOnce(Arc<Self>, TransferId) -> F,
    ) -> TransferId
    where
        F: std::future::Future<Output = Result<Outcome, Error>> + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let fut = f(self.clone(), id);
        let me = self.clone();
        // held until the task is registered, so one that ends at once can't unregister first
        let mut tasks = self.tasks.lock().unwrap();
        let task = rt.spawn(async move {
            let outcome = fut.await.unwrap_or_else(|e| match e {
                Error::Cancelled => {
                    eprintln!("qs: transfer {id}: the other device cancelled");
                    Outcome::Cancelled
                }
                e => {
                    eprintln!("qs: transfer {id} failed: {e}");
                    Outcome::Failed(e.to_string())
                }
            });
            match &outcome {
                Outcome::Received { files, texts, wifi } => {
                    eprintln!(
                        "qs: transfer {id} ended: received {} files, {} texts, {} networks",
                        files.len(),
                        texts.len(),
                        wifi.len()
                    )
                }
                o => eprintln!("qs: transfer {id} ended: {o:?}"),
            }
            me.task_done(id);
            me.emit(Event::Done { id, outcome });
        });
        tasks.insert(id, task.abort_handle());
        self.busy.send_replace(true);
        id
    }
}

pub struct Handle {
    inner: Arc<Inner>,
    port: u16,
    rt: tokio::runtime::Runtime,
    mdns: mdns::Mdns,
    ble: watch::Sender<bool>,
    visible: watch::Sender<bool>,
}

/// Start listening + discovery. Events arrive on the returned receiver
/// (runtime-agnostic, so it can be awaited from the GLib main loop).
pub fn start(settings: Settings) -> std::io::Result<(Handle, mpsc::UnboundedReceiver<Event>)> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let want = settings.port;
    let (inner, rx) = Inner::new(settings);
    let listener = rt.block_on(async {
        for port in [want, 0] {
            for host in ["[::]", "0.0.0.0"] {
                if let Ok(l) = TcpListener::bind(format!("{host}:{port}")).await {
                    return Ok(l);
                }
            }
        }
        TcpListener::bind("0.0.0.0:0").await
    })?;
    let port = listener.local_addr()?.port();
    eprintln!("qs: listening on TCP port {port}");
    let accept_inner = inner.clone();
    let rt_handle = rt.handle().clone();
    rt.spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            eprintln!("qs: incoming connection from {peer}");
            let _ = stream.set_nodelay(true);
            accept_inner.spawn(&rt_handle, |inner, id| async move {
                transfer::inbound(&inner, id, stream, Medium::Lan).await
            });
        }
    });
    let mdns = mdns::Mdns::start(inner.clone(), port).map_err(std::io::Error::other)?;
    let (ble, want) = watch::channel(false);
    rt.spawn(ble::run(inner.clone(), want));
    let (visible, vis_rx) = watch::channel(true);
    rt.spawn(bt::run(inner.clone(), vis_rx, ble.subscribe()));
    rt.spawn(hotspot::cleanup());
    Ok((
        Handle {
            inner,
            port,
            rt,
            mdns,
            ble,
            visible,
        },
        rx,
    ))
}

impl Handle {
    pub fn send(&self, device: Device, payload: Payload) -> TransferId {
        self.inner.spawn(self.rt.handle(), |inner, id| async move {
            transfer::outbound(&inner, id, device, payload).await
        })
    }

    /// Turn the Wi-Fi radio on; running Bluetooth transfers then move to the phone's Wi-Fi.
    pub fn turn_on_wifi(&self) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            match hotspot::turn_on().await {
                Ok(()) => {
                    eprintln!("qs: Wi-Fi turned on");
                    inner.wifi_on.send_modify(|n| *n += 1);
                }
                Err(e) => eprintln!("qs: couldn't turn Wi-Fi on: {e}"),
            }
        });
    }

    /// Abort a transfer; partial downloads are deleted.
    pub fn cancel(&self, id: TransferId) {
        if let Some(task) = self.inner.task_done(id) {
            eprintln!("qs: transfer {id} cancelled on this PC");
            task.abort();
            self.inner.emit(Event::Done {
                id,
                outcome: Outcome::Cancelled,
            });
        }
    }

    /// The TCP port actually in use (differs from `Settings::port` when that was taken).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether any other device has answered over mDNS since start (false on isolated networks).
    pub fn lan_active(&self) -> bool {
        self.mdns
            .lan_active
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_visible(&self, visible: bool) {
        self.mdns.set_visible(visible);
        let _ = self.visible.send(visible);
    }

    /// Call before exiting: hides us, which puts the Bluetooth adapter's settings back.
    pub fn shutdown(&self) {
        let _ = self.visible.send(false);
        // ponytail: fixed grace period for the Bluetooth task to restore; an ack channel if it's ever too short
        self.rt
            .block_on(async { tokio::time::sleep(std::time::Duration::from_millis(800)).await });
    }

    /// Bluetooth wake-up advert and scan, which make phones set to Everyone show up. Turn on while the user may send.
    pub fn set_wake_phones(&self, on: bool) {
        let _ = self.ble.send(on);
    }

    pub fn set_name(&self, name: String) {
        self.inner.settings.lock().unwrap().name = name;
        self.mdns.readvertise();
    }

    pub fn set_download_dir(&self, dir: PathBuf) {
        self.inner.settings.lock().unwrap().download_dir = dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback: our sender -> our receiver over localhost; covers framing, UKEY2, crypto, payloads.
    #[test]
    fn loopback_files_and_text() {
        let dir = std::env::temp_dir().join(format!("qs-test-{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(src.join("big.bin"), &big).unwrap();
        std::fs::write(src.join("note.txt"), b"hello").unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (inner, mut rx) = Inner::new(Settings {
            name: "Test PC".into(),
            download_dir: dir.join("dl"),
            port: 0,
        });
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let (h, i2) = (rt.handle().clone(), inner.clone());
        rt.spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                i2.spawn(&h, |inner, id| async move {
                    transfer::inbound(&inner, id, s, Medium::Lan).await
                });
            }
        });
        let dev = Device {
            id: "TEST".into(),
            name: "me".into(),
            kind: DeviceKind::Laptop,
            route: Route::Lan(addr),
        };

        let mut run = |payload: Payload| -> Vec<Outcome> {
            let dev = dev.clone();
            inner.spawn(rt.handle(), |inner, id| async move {
                transfer::outbound(&inner, id, dev, payload).await
            });
            let mut done = vec![];
            while done.len() < 2 {
                match rt.block_on(rx.recv()).unwrap() {
                    Event::Incoming(req) => {
                        assert_eq!(req.from, "Test PC");
                        assert_eq!(req.pin.len(), 4);
                        req.answer(true)
                    }
                    Event::Done { outcome, .. } => done.push(outcome),
                    _ => {}
                }
            }
            done
        };

        std::fs::create_dir_all(src.join("album/sub")).unwrap();
        std::fs::write(src.join("album/top.txt"), b"top").unwrap();
        std::fs::write(src.join("album/sub/deep.txt"), b"deep").unwrap();
        let received = |outcomes: &[Outcome]| {
            assert!(
                outcomes.iter().any(|o| matches!(o, Outcome::Sent)),
                "{outcomes:?}"
            );
            outcomes
                .iter()
                .find_map(|o| match o {
                    Outcome::Received { files, texts, .. } => Some((files.clone(), texts.clone())),
                    _ => None,
                })
                .expect("received")
        };

        // one mixed share: two files, a nested folder and a link
        let (files, texts) = received(&run(Payload {
            files: vec![src.join("big.bin"), src.join("note.txt"), src.join("album")],
            texts: vec!["https://example.com".into()],
        }));
        assert_eq!(std::fs::read(&files[0]).unwrap(), big);
        assert_eq!(std::fs::read(&files[1]).unwrap(), b"hello");
        assert_eq!(files[2], dir.join("dl/album"));
        assert_eq!(std::fs::read(files[2].join("top.txt")).unwrap(), b"top");
        assert_eq!(
            std::fs::read(files[2].join("sub/deep.txt")).unwrap(),
            b"deep"
        );
        assert_eq!(texts, ["https://example.com"]);

        // the same folder again lands next to it instead of merging
        let (files, _) = received(&run(Payload {
            files: vec![src.join("album")],
            ..Default::default()
        }));
        assert_eq!(files, [dir.join("dl/album (1)")]);
        assert_eq!(
            std::fs::read(files[0].join("sub/deep.txt")).unwrap(),
            b"deep"
        );

        assert_eq!(
            transfer::parse_endpoint_info(&transfer::endpoint_info("Ab"))
                .unwrap()
                .0,
            "Ab"
        );
        assert_eq!(transfer::safe_name("../../etc/passwd"), "passwd");
        assert_eq!(transfer::safe_name(".."), "file");
        assert_eq!(transfer::safe_folder("/a/../b\\c/"), ["a", "b", "c"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wifi_credentials() {
        use prost::Message;
        let meta = proto::share::WifiCredentialsMetadata {
            ssid: Some("Home".into()),
            security_type: Some(proto::share::wifi_credentials_metadata::SecurityType::Sae as i32),
            ..Default::default()
        };
        let data = proto::share::WifiCredentials {
            password: Some("secret".into()),
            hidden_ssid: Some(true),
        }
        .encode_to_vec();
        let w = transfer::wifi_network(&meta, &data).unwrap();
        assert_eq!(
            (w.ssid.as_str(), w.password.as_str(), w.security, w.hidden),
            ("Home", "secret", WifiSecurity::Sae, true)
        );
    }

    /// Two instances find each other over mDNS. Needs multicast, so it's opt-in:
    /// `cargo test -- --ignored mdns`
    #[test]
    #[ignore]
    fn mdns_discovery() {
        let dl = std::env::temp_dir();
        let (_a, mut rx_a) = start(Settings {
            name: "Alpha".into(),
            download_dir: dl.clone(),
            port: 0,
        })
        .unwrap();
        let (_b, _rx_b) = start(Settings {
            name: "Beta".into(),
            download_dir: dl,
            port: 0,
        })
        .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let found = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if let Some(Event::DeviceFound(d)) = rx_a.recv().await {
                        return d;
                    }
                }
            })
            .await
        });
        assert_eq!(found.expect("Beta not found within 10 s").name, "Beta");
    }
}
