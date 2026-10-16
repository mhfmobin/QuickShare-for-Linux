//! Bluetooth: finding each other and connecting without a shared Wi-Fi network.
//!
//! Formats from Google's Nearby Connections (connections/implementation), checked against a real phone:
//! - Being found: a BLE advertisement with service data under 0xFEF3 (mediums/ble/ble_advertisement.cc,
//!   version 2), wrapping the endpoint advertisement (ble_advertisement.cc) that carries the endpoint id,
//!   endpoint info and the classic Bluetooth address to connect to.
//! - Connecting: RFCOMM to the service whose UUID is derived from "NearbySharing"; the same frames as
//!   on a LAN then flow over that socket.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bluer::adv::{Advertisement, AdvertisementHandle, SecondaryChannel, Type};
use bluer::rfcomm::{Profile, ProfileHandle, Role, Stream};
use bluer::{AdapterEvent, Address, DiscoveryFilter, DiscoveryTransport, Uuid};
use futures::StreamExt;
use tokio::sync::watch;

use crate::mdns::SERVICE_ID;
use crate::transfer::{endpoint_info, parse_endpoint_info};
use crate::{chan::tracing, transfer, Device, Error, Event, Inner, Medium, Route};

/// Java's `UUID.nameUUIDFromBytes("NearbySharing")`, which Nearby uses as the RFCOMM service.
pub const SERVICE: Uuid = Uuid::from_u128(0xa82efa21_ae5c_3dde_9bbc_f16da7b16c5a);
/// Nearby's ("Copresence") BLE service.
const FEF3: Uuid = Uuid::from_u128(0x0000fef3_0000_1000_8000_00805f9b34fb);
/// A phone that stopped advertising for this long is gone.
const LOST_AFTER: Duration = Duration::from_secs(60);
/// Scan in bursts: continuous LE scanning can starve the page scan that lets phones connect to us.
const SCAN_BURST: Duration = Duration::from_secs(6);

/// While visible we're also discoverable over classic Bluetooth: that's what makes Linux accept
/// connections from unpaired phones ("connectable"), which BlueZ has no separate switch for.
/// BlueZ turns it off by itself after this long, so a crash doesn't leave it on; we keep renewing it.
const DISCOVERABLE_FOR: u32 = 180;

/// How long a classic Bluetooth connection may take to come up (paging a phone can take seconds).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

fn fail(msg: impl Into<String>) -> Error {
    Error::Io(std::io::Error::other(msg.into()))
}

/// The 0xFEF3 service data advertising us:
/// `[0x48: v2, socket v2][service id hash][u32 length][endpoint advertisement][2-byte device token]`,
/// endpoint advertisement = `[0x23: v1, P2P_POINT_TO_POINT][hash][endpoint id][info len][info][BT address][0][0]`.
pub fn advertisement(endpoint_id: &[u8; 4], info: &[u8], bt_address: [u8; 6]) -> Vec<u8> {
    let info = &info[..info.len().min(131)];
    let mut endpoint = vec![0x23];
    endpoint.extend_from_slice(&SERVICE_ID);
    endpoint.extend_from_slice(endpoint_id);
    endpoint.push(info.len() as u8);
    endpoint.extend_from_slice(info);
    endpoint.extend_from_slice(&bt_address);
    endpoint.extend_from_slice(&[0, 0]); // no UWB address, not WebRTC-connectable
    let mut b = vec![0x48];
    b.extend_from_slice(&SERVICE_ID);
    b.extend_from_slice(&(endpoint.len() as u32).to_be_bytes());
    b.extend_from_slice(&endpoint);
    b.extend_from_slice(&rand::random::<[u8; 2]>());
    b
}

/// (endpoint id, endpoint info, classic Bluetooth address) from a phone's 0xFEF3 service data.
pub fn parse_advertisement(b: &[u8]) -> Option<(String, Vec<u8>, [u8; 6])> {
    // "fast" advertisements (bit 1) carry no address to connect to
    if b.len() < 8 || b[0] & 0x02 != 0 || b[1..4] != SERVICE_ID {
        return None;
    }
    let len = u32::from_be_bytes(b[4..8].try_into().ok()?) as usize;
    let e = b.get(8..8 + len)?;
    if e.len() < 9 || e[0] >> 5 != 1 || e[1..4] != SERVICE_ID {
        return None;
    }
    let n = e[8] as usize;
    let info = e.get(9..9 + n)?;
    let addr: [u8; 6] = e.get(9 + n..15 + n)?.try_into().ok()?;
    (addr != [0; 6]).then(|| {
        (
            String::from_utf8_lossy(&e[4..8]).into_owned(),
            info.to_vec(),
            addr,
        )
    })
}

/// Earlier versions renamed the adapter while visible; put its own name back if that's still pending.
async fn restore_name(adapter: &bluer::Adapter) {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default();
    let path = config.join("quickshare-linux").join("bluetooth-name");
    let Ok(saved) = std::fs::read_to_string(&path) else {
        return;
    };
    let (discoverable, alias) = saved.split_once('\n').unwrap_or(("0", ""));
    if adapter.set_alias(alias.to_owned()).await.is_ok()
        && adapter.set_discoverable(discoverable == "1").await.is_ok()
    {
        let _ = std::fs::remove_file(path);
    }
}

async fn next_conn(h: &mut Option<ProfileHandle>) -> Option<bluer::rfcomm::ConnectRequest> {
    match h {
        Some(h) => h.next().await,
        None => std::future::pending().await,
    }
}

type Discovery = Pin<Box<dyn futures::Stream<Item = AdapterEvent> + Send>>;

async fn next_found(d: &mut Option<Discovery>) -> Option<AdapterEvent> {
    match d {
        Some(d) => d.next().await,
        None => std::future::pending().await,
    }
}

/// A phone's Nearby advertisement, if this BLE device is sending one.
/// Other 0xFEF3 adverts (e.g. "fast" ones without an address) are logged once per device.
async fn nearby_device(
    adapter: &bluer::Adapter,
    addr: Address,
    logged: &mut HashSet<Address>,
) -> Option<Device> {
    let data = adapter.device(addr).ok()?.service_data().await.ok()??;
    let bytes = data.get(&FEF3)?;
    let Some((id, info, bt)) = parse_advertisement(bytes) else {
        if tracing() && logged.insert(addr) {
            eprintln!("qs: BLE {addr} fef3 not understood: {bytes:02x?}");
        }
        return None;
    };
    let (name, kind) = parse_endpoint_info(&info)?;
    Some(Device {
        id,
        name,
        kind,
        route: Route::Bluetooth(bt),
    })
}

async fn start_advertising(
    inner: &Inner,
    adapter: &bluer::Adapter,
) -> bluer::Result<AdvertisementHandle> {
    let name = inner.settings.lock().unwrap().name.clone();
    let data = advertisement(
        &inner.endpoint_id,
        &endpoint_info(&name),
        adapter.address().await?.0,
    );
    adapter
        .advertise(Advertisement {
            advertisement_type: Type::Broadcast,
            service_data: [(FEF3, data)].into(),
            // longer than a legacy advert, like the phone's own
            secondary_channel: Some(SecondaryChannel::OneM),
            min_interval: Some(Duration::from_millis(150)),
            max_interval: Some(Duration::from_millis(300)),
            ..Default::default()
        })
        .await
}

/// Bluetooth for the app's lifetime: advertising while visible, the RFCOMM service, scanning while sending.
pub async fn run(
    inner: Arc<Inner>,
    mut visible: watch::Receiver<bool>,
    mut scan: watch::Receiver<bool>,
) {
    let Ok(session) = bluer::Session::new().await else {
        return;
    };
    if let Ok(a) = session.default_adapter().await {
        restore_name(&a).await;
    }
    let mut service = None;
    for channel in [21, 22, 23, 24, 0] {
        let profile = Profile {
            uuid: SERVICE,
            name: Some("NearbySharing".into()),
            role: Some(Role::Server),
            channel: (channel > 0).then_some(channel),
            require_authentication: Some(false),
            require_authorization: Some(false),
            ..Default::default()
        };
        match session.register_profile(profile).await {
            Ok(h) => {
                service = Some(h);
                break;
            }
            Err(e) => eprintln!("qs: RFCOMM service on channel {channel}: {e}"),
        }
    }
    let mut advert: Option<(String, AdvertisementHandle)> = None; // (name advertised, handle)
    let mut saved_timeout: Option<u32> = None; // set while we keep the adapter discoverable
    let mut discovery: Option<Discovery> = None;
    let mut next_toggle = Instant::now(); // when the scan burst (or the pause after it) ends
    let mut phones: HashMap<String, Instant> = HashMap::new(); // endpoint id -> last seen
    let mut logged = HashSet::new();
    let mut busy = inner.busy.subscribe();
    loop {
        let adapter = session.default_adapter().await.ok();
        let powered = match &adapter {
            Some(a) => a.is_powered().await.unwrap_or(false),
            None => false,
        };
        // no LE scans or adverts during transfers (see Inner::busy)
        let quiet = *busy.borrow();
        let want_scan = *scan.borrow() && powered && !quiet;
        if let Some(a) = &adapter {
            let name = inner.settings.lock().unwrap().name.clone();
            if *visible.borrow() && powered {
                if !a.is_discoverable().await.unwrap_or(true) {
                    let before = a.discoverable_timeout().await.unwrap_or(0);
                    let ok = a.set_discoverable_timeout(DISCOVERABLE_FOR).await.is_ok()
                        && a.set_discoverable(true).await.is_ok();
                    if ok && saved_timeout.is_none() {
                        saved_timeout = Some(before);
                    }
                }
            } else if let Some(before) = saved_timeout.take() {
                let restored = a
                    .set_discoverable(false)
                    .await
                    .and(a.set_discoverable_timeout(before).await);
                if let Err(e) = restored {
                    eprintln!("qs: couldn't restore Bluetooth discoverability: {e}");
                }
            }
            if !(*visible.borrow() && powered) || quiet {
                advert = None; // dropping the handle stops advertising
            } else if advert.as_ref().map(|(n, _)| n) != Some(&name) {
                advert = None;
                match start_advertising(&inner, a).await {
                    Ok(h) => {
                        eprintln!("qs: visible over Bluetooth");
                        advert = Some((name, h));
                    }
                    Err(e) => eprintln!("qs: Bluetooth advertising failed: {e}"),
                }
            }
            if !want_scan {
                discovery = None;
            } else if Instant::now() >= next_toggle {
                next_toggle = Instant::now() + SCAN_BURST;
                // alternate: a burst of scanning, then a pause of the same length
                if discovery.take().is_none() {
                    let filter = DiscoveryFilter {
                        transport: DiscoveryTransport::Le,
                        duplicate_data: true,
                        ..Default::default()
                    };
                    let _ = a.set_discovery_filter(filter).await;
                    discovery = a
                        .discover_devices_with_changes()
                        .await
                        .ok()
                        .map(|s| Box::pin(s) as Discovery);
                }
            }
        }
        phones.retain(|id, seen| {
            let alive = seen.elapsed() < LOST_AFTER;
            if !alive {
                inner.emit(Event::DeviceLost(id.clone()));
            }
            alive
        });
        // ponytail: polls adapter state every 5 s; switch to adapter.events() if that lag matters
        tokio::select! {
            r = visible.changed() => if r.is_err() { break },
            r = scan.changed() => if r.is_err() { break },
            r = busy.changed() => if r.is_err() { break },
            Some(req) = next_conn(&mut service) => {
                let from = req.device();
                match req.accept() {
                    Ok(stream) => {
                        eprintln!("qs: incoming Bluetooth connection from {from}");
                        inner.spawn(&tokio::runtime::Handle::current(), |inner, id| async move {
                            transfer::inbound(&inner, id, stream, Medium::Bluetooth).await
                        });
                    }
                    Err(e) => eprintln!("qs: Bluetooth connection from {from} failed: {e}"),
                }
            }
            Some(ev) = next_found(&mut discovery) => {
                if let (AdapterEvent::DeviceAdded(addr), Some(a)) = (ev, &adapter) {
                    if let Some(dev) = nearby_device(a, addr, &mut logged).await {
                        if dev.id.as_bytes() != inner.endpoint_id
                            && phones.insert(dev.id.clone(), Instant::now()).is_none()
                        {
                            eprintln!("qs: found {} over Bluetooth", dev.name);
                            inner.emit(Event::DeviceFound(dev));
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = tokio::time::sleep_until(next_toggle.into()), if want_scan => {}
        }
    }
}

/// ServiceSearchAttributeRequest for our service's ProtocolDescriptorList (attribute 0x0004).
fn sdp_request(tid: u16, continuation: &[u8]) -> Vec<u8> {
    let mut params = vec![0x35, 0x11, 0x1c]; // search pattern: one UUID-128
    params.extend_from_slice(SERVICE.as_bytes());
    params.extend_from_slice(&0x0400u16.to_be_bytes()); // max attribute bytes per response
    params.extend_from_slice(&[0x35, 0x03, 0x09, 0x00, 0x04]); // attribute ids: 0x0004
    params.push(continuation.len() as u8);
    params.extend_from_slice(continuation);
    let mut pdu = vec![0x06];
    pdu.extend_from_slice(&tid.to_be_bytes());
    pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
    pdu.extend(params);
    pdu
}

/// The RFCOMM channel in a ProtocolDescriptorList: UUID-16 0x0003 (RFCOMM) followed by a uint8.
// ponytail: byte-pattern match instead of a full SDP data-element parser
fn rfcomm_channel(attributes: &[u8]) -> Option<u8> {
    attributes
        .windows(5)
        .find(|w| w[..4] == [0x19, 0x00, 0x03, 0x08])
        .map(|w| w[4])
}

/// bluer's connect() returns before the link is up: a fresh Bluetooth socket already polls as
/// writable, so its readiness check passes at once (sending then fails with ENOTCONN).
/// The kernel only reports a connecting socket writable once connected, so wait for that here.
async fn connected(fd: RawFd) -> Result<(), Error> {
    let res = tokio::task::spawn_blocking(move || {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut p, 1, CONNECT_TIMEOUT.as_millis() as i32) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n == 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        if p.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            let mut err: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut err as *mut _ as *mut libc::c_void,
                    &mut len,
                );
            }
            return Err(match err {
                0 => std::io::ErrorKind::ConnectionRefused.into(),
                e => std::io::Error::from_raw_os_error(e),
            });
        }
        Ok(())
    })
    .await
    .map_err(|_| Error::Closed)?;
    Ok(res?)
}

/// Ask the phone (over SDP) which RFCOMM channel its Nearby service listens on.
/// BlueZ would do this for us, but only for devices it found in a classic scan,
/// and phones are only found over BLE.
async fn sdp_channel(addr: Address) -> Result<u8, Error> {
    use bluer::l2cap::{SeqPacket, SocketAddr};
    let sock = SeqPacket::connect(SocketAddr::new(addr, bluer::AddressType::BrEdr, 1)).await?;
    connected(sock.as_raw_fd()).await?;
    let (mut attributes, mut continuation, mut buf) = (vec![], vec![], vec![0u8; 2048]);
    for tid in 1.. {
        sock.send(&sdp_request(tid, &continuation)).await?;
        let n = sock.recv(&mut buf).await?;
        let r = &buf[..n];
        if r.len() < 7 || r[0] != 0x07 {
            return Err(fail("the phone's Bluetooth service list couldn't be read"));
        }
        let count = u16::from_be_bytes([r[5], r[6]]) as usize;
        let rest = r.get(7..).unwrap_or_default();
        attributes.extend_from_slice(rest.get(..count).ok_or_else(|| fail("bad SDP reply"))?);
        let cont_len = *rest.get(count).unwrap_or(&0) as usize;
        continuation = rest
            .get(count + 1..count + 1 + cont_len)
            .unwrap_or_default()
            .to_vec();
        if continuation.is_empty() || tid > 8 {
            break;
        }
    }
    rfcomm_channel(&attributes)
        .ok_or_else(|| fail("the phone isn't accepting Quick Share over Bluetooth"))
}

/// Open an RFCOMM connection to a phone found over Bluetooth.
pub async fn connect(addr: [u8; 6]) -> Result<Stream, Error> {
    let addr = Address(addr);
    let timeout = |what: &'static str| move |_| Error::Protocol(what);
    let channel = tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(5), sdp_channel(addr))
        .await
        .map_err(timeout("the phone didn't answer over Bluetooth"))?
        .map_err(|e| fail(format!("couldn't reach the phone over Bluetooth: {e}")))?;
    eprintln!("qs: connecting to {addr} on RFCOMM channel {channel}");
    let stream = Stream::connect(bluer::rfcomm::SocketAddr::new(addr, channel)).await?;
    connected(stream.as_raw_fd()).await.map_err(|e| {
        fail(format!(
            "the phone didn't accept the Bluetooth connection: {e}"
        ))
    })?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertisement_roundtrip() {
        let info = endpoint_info("Test PC");
        let bt = [0x8c, 0xc5, 0xd0, 0xb1, 0x60, 0x09];
        let (id, back, addr) = parse_advertisement(&advertisement(b"AbCd", &info, bt)).unwrap();
        assert_eq!((id.as_str(), back, addr), ("AbCd", info, bt));
    }

    /// Same layout as a real phone's advert, including the trailing device token and PSM extra field.
    #[test]
    fn phone_advertisement() {
        let info = [&[0x22][..], &[7; 16], &[5], b"Phone", &[2, 1, 1]].concat();
        let endpoint = [
            &[0x23, 0xfc, 0x9f, 0x5e][..],
            b"CHJM",
            &[info.len() as u8],
            &info,
            &[0x8c, 0xc5, 0xd0, 0xb1, 0x60, 0x09, 0, 0],
        ]
        .concat();
        let advert = [
            &[0x48, 0xfc, 0x9f, 0x5e][..],
            &(endpoint.len() as u32).to_be_bytes(),
            &endpoint,
            &[0x1a, 0x59, 0x01, 0x00, 0x8d],
        ]
        .concat();
        let (id, info, addr) = parse_advertisement(&advert).unwrap();
        assert_eq!(
            (id.as_str(), addr),
            ("CHJM", [0x8c, 0xc5, 0xd0, 0xb1, 0x60, 0x09])
        );
        assert_eq!(parse_endpoint_info(&info).unwrap().0, "Phone");
        assert!(parse_advertisement(&advert[..20]).is_none());
    }

    /// Needs a real adapter: `cargo test -- --ignored bt_connect_waits`. Before the fix this failed
    /// at once with ENOTCONN; now it waits for paging to time out and reports why.
    #[test]
    #[ignore]
    fn bt_connect_waits() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let start = Instant::now();
        let err = rt
            .block_on(sdp_channel(Address([0x00, 0x1a, 0x7d, 0xda, 0x71, 0x0f])))
            .unwrap_err();
        let msg = err.to_string();
        eprintln!("after {:?}: {msg}", start.elapsed());
        assert!(!msg.contains("os error 107"), "{msg}");
        assert!(start.elapsed() > Duration::from_secs(1));
    }

    #[test]
    fn sdp_rfcomm_channel() {
        // [[L2CAP], [RFCOMM, channel 12]]
        let attrs = [
            0x35, 0x11, 0x35, 0x0f, 0x09, 0x00, 0x04, 0x35, 0x0a, 0x35, 0x03, 0x19, 0x01, 0x00,
            0x35, 0x05, 0x19, 0x00, 0x03, 0x08, 0x0c,
        ];
        assert_eq!(rfcomm_channel(&attrs), Some(12));
        assert_eq!(rfcomm_channel(&attrs[..10]), None);
        assert_eq!(sdp_request(1, &[])[..3], [0x06, 0x00, 0x01]);
    }
}
