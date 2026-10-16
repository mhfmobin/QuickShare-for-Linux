//! Framed, optionally encrypted connection + the Nearby "payload" layer on top.
//! Runs over any byte stream (TCP on a LAN, RFCOMM over Bluetooth) and can move a
//! Bluetooth connection onto the phone's Wi-Fi hotspot mid-transfer ("bandwidth upgrade").

use std::collections::HashMap;
use std::future::{pending, Future};
use std::io;
use std::pin::Pin;
use std::time::Duration;

use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::{sleep_until, Instant};

use crate::crypto::Keys;
use crate::proto::connections as nc;
use crate::proto::share as sharing;
use crate::{hotspot, Error, Medium};

use nc::bandwidth_upgrade_negotiation_frame::{
    upgrade_path_info, ClientIntroduction, EventType as Bwu, UpgradePathInfo,
};
use nc::payload_transfer_frame::{
    payload_header::PayloadType, PacketType, PayloadChunk, PayloadHeader,
};
use nc::v1_frame::FrameType;

const MAX_FRAME: usize = 5 * 1024 * 1024;
const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// Anything a connection can run over.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

type Reader = mpsc::Receiver<io::Result<Vec<u8>>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;
type Joined = Result<Result<(TcpStream, hotspot::Wifi), Error>, JoinError>;

pub enum In {
    Bytes(i64, Vec<u8>),
    File {
        id: i64,
        offset: i64,
        body: Vec<u8>,
        last: bool,
    },
    Disconnected,
}

/// Moving onto the phone's hotspot. The phone always hosts; we join.
enum Upgrade {
    Idle,
    /// Offered before the user accepted; joined once allowed.
    Offered(UpgradePathInfo),
    Joining(
        JoinHandle<Result<(TcpStream, hotspot::Wifi), Error>>,
        UpgradePathInfo,
    ),
    /// The new connection is up. Both sides send LAST_WRITE, answer it with SAFE_TO_CLOSE,
    /// and switch once they get the other's SAFE_TO_CLOSE (as in Nearby's BwuManager).
    Switching {
        rx: Reader,
        w: Writer,
        wifi: hotspot::Wifi,
        sent_safe_to_close: bool,
        /// Both sides agreed to drop encryption on the new link (it's WPA2 already).
        plain: bool,
    },
}

enum Wake<T> {
    Other(T),
    Frame(Option<io::Result<Vec<u8>>>),
    Joined(Joined),
    KeepAlive,
    WifiOn,
}

pub struct Chan {
    rx: Reader,
    w: Writer,
    pub keys: Option<Keys>,
    send_seq: i32,
    recv_seq: i32,
    buffers: HashMap<i64, Vec<u8>>,
    last_sent: Instant,
    medium: Medium,
    endpoint_id: String,
    upgrade: Upgrade,
    allow_upgrade: bool,
    peer_last_write: bool,
    /// Keeps us on the phone's hotspot; going back to the old Wi-Fi when dropped.
    _wifi: Option<hotspot::Wifi>,
    /// Told when the connection moves to another medium.
    pub on_medium: Option<Box<dyn Fn(Medium) + Send>>,
    /// Ticks when the user turned the laptop's Wi-Fi on mid-transfer.
    pub wifi_on: Option<watch::Receiver<u32>>,
}

async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

fn framed(data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(data.len() + 4);
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

/// A reader task makes receiving cancel-safe, so we can race it against timers/consent.
fn split(stream: impl Stream) -> (Reader, Writer) {
    let (mut r, w) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        loop {
            let res = read_frame(&mut r).await;
            let failed = res.is_err();
            if tx.send(res).await.is_err() || failed {
                break;
            }
        }
    });
    (rx, Box::new(w))
}

pub fn offline(v1: nc::V1Frame) -> nc::OfflineFrame {
    nc::OfflineFrame {
        version: Some(nc::offline_frame::Version::V1 as i32),
        v1: Some(v1),
    }
}

fn bwu(f: nc::BandwidthUpgradeNegotiationFrame) -> nc::OfflineFrame {
    offline(nc::V1Frame {
        r#type: Some(FrameType::BandwidthUpgradeNegotiation as i32),
        bandwidth_upgrade_negotiation: Some(f),
        ..Default::default()
    })
}

fn bwu_event(ty: Bwu) -> nc::OfflineFrame {
    bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(ty as i32),
        ..Default::default()
    })
}

fn bwu_failure(info: UpgradePathInfo) -> nc::OfflineFrame {
    bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(Bwu::UpgradeFailure as i32),
        upgrade_path_info: Some(info),
        ..Default::default()
    })
}

fn disconnection() -> nc::OfflineFrame {
    offline(nc::V1Frame {
        r#type: Some(FrameType::Disconnection as i32),
        disconnection: Some(Default::default()),
        ..Default::default()
    })
}

/// What we can do on a direct Wi-Fi link: join the phone's network (5 GHz is fine).
pub fn client_metadata() -> nc::MediumMetadata {
    nc::MediumMetadata {
        supports_5_ghz: Some(true),
        medium_role: Some(nc::MediumRole {
            support_wifi_hotspot_client: Some(true),
            support_wifi_direct_group_client: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// CLIENT_INTRODUCTION (and its ACK) go unencrypted on the fresh connection, like Nearby does.
/// `plain`: the phone offered to drop encryption on the new link and we agree (Android does so
/// even when told no, sending plaintext after the switch).
async fn introduce(
    res: Joined,
    endpoint_id: String,
    ack: bool,
    plain: bool,
) -> Result<(Reader, Writer, hotspot::Wifi), Error> {
    let (mut stream, wifi) = res.map_err(|_| Error::Closed)??;
    let intro = bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(Bwu::ClientIntroduction as i32),
        client_introduction: Some(ClientIntroduction {
            endpoint_id: Some(endpoint_id.clone()),
            supports_disabling_encryption: Some(plain),
            last_endpoint_id: Some(endpoint_id.clone()),
        }),
        ..Default::default()
    });
    stream.write_all(&framed(&intro.encode_to_vec())).await?;
    if ack {
        let raw = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut stream))
            .await
            .map_err(|_| Error::Protocol("no CLIENT_INTRODUCTION_ACK"))??;
        let f = nc::OfflineFrame::decode(&*raw)?.v1.unwrap_or_default();
        let ty = f
            .bandwidth_upgrade_negotiation
            .unwrap_or_default()
            .event_type();
        if ty != Bwu::ClientIntroductionAck {
            return Err(Error::Protocol("expected CLIENT_INTRODUCTION_ACK"));
        }
    }
    let _ = stream.set_nodelay(true);
    let (rx, w) = split(stream);
    Ok((rx, w, wifi))
}

pub fn payload_frame(
    id: i64,
    ty: PayloadType,
    total: i64,
    offset: i64,
    body: Vec<u8>,
    last: bool,
) -> nc::OfflineFrame {
    offline(nc::V1Frame {
        r#type: Some(FrameType::PayloadTransfer as i32),
        payload_transfer: Some(nc::PayloadTransferFrame {
            packet_type: Some(PacketType::Data as i32),
            payload_header: Some(PayloadHeader {
                id: Some(id),
                r#type: Some(ty as i32),
                total_size: Some(total),
                is_sensitive: Some(false),
                ..Default::default()
            }),
            payload_chunk: Some(PayloadChunk {
                flags: Some(last as i32),
                offset: Some(offset),
                body: Some(body),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

impl Chan {
    pub fn new(stream: impl Stream, medium: Medium, endpoint_id: String) -> Self {
        let (rx, w) = split(stream);
        Chan {
            rx,
            w,
            keys: None,
            send_seq: 0,
            recv_seq: 0,
            buffers: HashMap::new(),
            last_sent: Instant::now(),
            medium,
            endpoint_id,
            upgrade: Upgrade::Idle,
            allow_upgrade: false,
            peer_last_write: false,
            _wifi: None,
            on_medium: None,
            wifi_on: None,
        }
    }

    pub fn medium(&self) -> Medium {
        self.medium
    }

    pub async fn send_raw(&mut self, data: &[u8]) -> Result<(), Error> {
        self.w.write_all(&framed(data)).await?;
        self.last_sent = Instant::now();
        Ok(())
    }

    /// Unencrypted read; only used during the handshake.
    pub async fn recv_raw(&mut self) -> Result<Vec<u8>, Error> {
        Ok(self.rx.recv().await.ok_or(Error::Closed)??)
    }

    /// Send a frame. While switching to the hotspot nothing else may go out on the old
    /// connection, so this waits for the switch to finish first.
    pub async fn send(&mut self, frame: nc::OfflineFrame) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while matches!(self.upgrade, Upgrade::Switching { .. }) {
            if Instant::now() > deadline {
                return Err(Error::Protocol("the switch to the phone's Wi-Fi stalled"));
            }
            let w = self.wait(std::pin::pin!(pending::<()>())).await;
            match self.react(w).await? {
                Some(In::Bytes(_, data)) => check_cancel(&data)?,
                Some(In::Disconnected) => return Err(Error::Closed),
                _ => {}
            }
        }
        self.write_frame(frame).await
    }

    async fn write_frame(&mut self, frame: nc::OfflineFrame) -> Result<(), Error> {
        if let Some(v1) = &frame.v1 {
            trace("->", v1);
        }
        let bytes = frame.encode_to_vec();
        let data = match &self.keys {
            None => bytes,
            Some(k) => {
                self.send_seq += 1;
                k.seal(self.send_seq, bytes)
            }
        };
        self.send_raw(&data).await
    }

    pub async fn recv(&mut self) -> Result<nc::V1Frame, Error> {
        let raw = self.recv_raw().await?;
        self.decode(raw)
    }

    fn decode(&mut self, raw: Vec<u8>) -> Result<nc::V1Frame, Error> {
        let bytes = match &self.keys {
            None => raw,
            Some(k) => match k.open(self.recv_seq + 1, &raw) {
                Ok(bytes) => {
                    self.recv_seq += 1;
                    bytes
                }
                Err(e) => {
                    // Nearby sends some goodbyes unencrypted: accept a plain DISCONNECTION, nothing else.
                    let plain = nc::OfflineFrame::decode(&*raw).ok().and_then(|f| f.v1);
                    if let Some(v1) = plain.filter(|v1| v1.r#type() == FrameType::Disconnection) {
                        trace("<-", &v1);
                        return Ok(v1);
                    }
                    eprintln!(
                        "qs: undecodable frame ({} bytes): {:02x?}",
                        raw.len(),
                        &raw[..raw.len().min(48)]
                    );
                    return Err(e);
                }
            },
        };
        let v1 = nc::OfflineFrame::decode(&*bytes)?
            .v1
            .ok_or(Error::Protocol("missing v1 frame"))?;
        trace("<-", &v1);
        Ok(v1)
    }

    async fn keep_alive(&mut self, ack: bool) -> Result<(), Error> {
        self.write_frame(offline(nc::V1Frame {
            r#type: Some(FrameType::KeepAlive as i32),
            keep_alive: Some(nc::KeepAliveFrame {
                ack: Some(ack),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
    }

    pub async fn disconnect(&mut self) {
        let _ = self.send(disconnection()).await;
        let _ = self.w.shutdown().await;
        // Drain until the peer closes, so unread data never turns our close into a RST.
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(raw)) = self.rx.recv().await {
                let _ = self.decode(raw); // decode only to trace it
            }
        })
        .await;
    }

    /// Bytes payloads are sent as one data chunk plus an empty last chunk, like Android does.
    pub async fn send_bytes(&mut self, id: i64, data: Vec<u8>) -> Result<(), Error> {
        let len = data.len() as i64;
        self.send(payload_frame(id, PayloadType::Bytes, len, 0, data, false))
            .await?;
        self.send(payload_frame(
            id,
            PayloadType::Bytes,
            len,
            len,
            vec![],
            true,
        ))
        .await
    }

    pub async fn send_sharing(&mut self, v1: sharing::V1Frame) -> Result<(), Error> {
        let frame = sharing::Frame {
            version: Some(sharing::frame::Version::V1 as i32),
            v1: Some(v1),
        };
        self.send_bytes(rand::random(), frame.encode_to_vec()).await
    }

    // ---- moving to the phone's hotspot ----

    /// The user agreed to leave the current Wi-Fi: join a hotspot the phone offers (or offered).
    pub fn allow_upgrade(&mut self) {
        self.allow_upgrade = true;
        if let Upgrade::Offered(info) = std::mem::replace(&mut self.upgrade, Upgrade::Idle) {
            self.start_join(info);
        }
    }

    /// Ask the phone to host a hotspot for us. Nearby lets the side that accepted the
    /// connection pick the upgrade, which is us when the phone sends; this hands it back.
    pub async fn request_upgrade(&mut self) -> Result<(), Error> {
        use upgrade_path_info::Medium as M;
        self.write_frame(bwu(nc::BandwidthUpgradeNegotiationFrame {
            event_type: Some(Bwu::UpgradePathRequest as i32),
            upgrade_path_info: Some(UpgradePathInfo {
                medium: Some(M::WifiHotspot as i32),
                upgrade_path_request: Some(upgrade_path_info::UpgradePathRequest {
                    mediums: vec![M::WifiHotspot as i32, M::WifiDirect as i32],
                    medium_meta_data: Some(client_metadata()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
    }

    /// Wi-Fi was off and the user turned it on: ask the phone for its network now.
    async fn on_wifi_on(&mut self) -> Result<(), Error> {
        if self.medium == Medium::Bluetooth
            && self.allow_upgrade
            && matches!(self.upgrade, Upgrade::Idle)
        {
            eprintln!("qs: Wi-Fi is on now, asking the phone to switch");
            self.request_upgrade().await?;
        }
        Ok(())
    }

    fn start_join(&mut self, info: UpgradePathInfo) {
        eprintln!("qs: joining the phone's Wi-Fi for a faster transfer");
        self.upgrade = Upgrade::Joining(tokio::spawn(hotspot::join(info.clone())), info);
    }

    /// The hotspot join finished: introduce ourselves on the new connection, then start the switch.
    async fn on_joined(&mut self, res: Joined) -> Result<(), Error> {
        let Upgrade::Joining(_, info) = std::mem::replace(&mut self.upgrade, Upgrade::Idle) else {
            return Ok(());
        };
        let (ack, plain) = (
            info.supports_client_introduction_ack(),
            info.supports_disabling_encryption(),
        );
        match introduce(res, self.endpoint_id.clone(), ack, plain).await {
            Ok((rx, w, wifi)) => {
                self.write_frame(bwu_event(Bwu::LastWriteToPriorChannel))
                    .await?;
                self.upgrade = Upgrade::Switching {
                    rx,
                    w,
                    wifi,
                    sent_safe_to_close: false,
                    plain,
                };
                if self.peer_last_write {
                    self.send_safe_to_close().await?;
                }
            }
            Err(e) => {
                eprintln!("qs: couldn't move to the phone's Wi-Fi, staying on Bluetooth: {e}");
                self.write_frame(bwu_failure(info)).await?;
            }
        }
        Ok(())
    }

    async fn send_safe_to_close(&mut self) -> Result<(), Error> {
        if let Upgrade::Switching {
            sent_safe_to_close: sent @ false,
            ..
        } = &mut self.upgrade
        {
            *sent = true;
            self.write_frame(bwu_event(Bwu::SafeToClosePriorChannel))
                .await?;
        }
        Ok(())
    }

    /// The phone is done with the old connection: say goodbye on it (unencrypted, as Nearby does) and move over.
    async fn finish_switch(&mut self) -> Result<(), Error> {
        let Upgrade::Switching {
            rx, w, wifi, plain, ..
        } = std::mem::replace(&mut self.upgrade, Upgrade::Idle)
        else {
            return Ok(());
        };
        let _ = self.send_raw(&disconnection().encode_to_vec()).await;
        let _ = self.w.shutdown().await;
        self.rx = rx;
        self.w = w;
        self._wifi = Some(wifi);
        if plain {
            self.keys = None;
        }
        self.medium = Medium::Direct;
        eprintln!("qs: transfer moved to the phone's Wi-Fi");
        if let Some(f) = &self.on_medium {
            f(Medium::Direct);
        }
        Ok(())
    }

    async fn on_bwu(&mut self, f: nc::BandwidthUpgradeNegotiationFrame) -> Result<(), Error> {
        match f.event_type() {
            Bwu::UpgradePathAvailable => {
                let info = f.upgrade_path_info.unwrap_or_default();
                // On a LAN we're already on Wi-Fi; never trade it for a hotspot.
                if self.medium != Medium::Bluetooth || !matches!(self.upgrade, Upgrade::Idle) {
                    self.write_frame(bwu_failure(info)).await?;
                } else if self.allow_upgrade {
                    self.start_join(info);
                } else {
                    self.upgrade = Upgrade::Offered(info);
                }
            }
            Bwu::LastWriteToPriorChannel => {
                self.peer_last_write = true;
                self.send_safe_to_close().await?;
            }
            Bwu::SafeToClosePriorChannel => self.finish_switch().await?,
            Bwu::UpgradeFailure => {
                eprintln!("qs: the phone couldn't set up its Wi-Fi; staying on Bluetooth");
            }
            _ => {}
        }
        Ok(())
    }

    // ---- receiving ----

    /// Wait for whichever comes first. Every branch is cancel-safe.
    async fn wait<T>(&mut self, other: Pin<&mut impl Future<Output = T>>) -> Wake<T> {
        let keep_alive_at = self.last_sent + KEEP_ALIVE;
        let up = &mut self.upgrade;
        let joined = async move {
            match up {
                Upgrade::Joining(h, _) => h.await,
                _ => pending().await,
            }
        };
        let wifi = &mut self.wifi_on;
        let wifi_on = async move {
            let ticked = match wifi {
                Some(rx) => rx.changed().await.is_ok(),
                None => false,
            };
            if !ticked {
                pending::<()>().await; // no signal (or it's gone): never wake
            }
        };
        tokio::select! {
            t = other => Wake::Other(t),
            raw = self.rx.recv() => Wake::Frame(raw),
            j = joined => Wake::Joined(j),
            _ = wifi_on => Wake::WifiOn,
            _ = sleep_until(keep_alive_at) => Wake::KeepAlive,
        }
    }

    async fn react<T>(&mut self, w: Wake<T>) -> Result<Option<In>, Error> {
        match w {
            Wake::Other(_) => Ok(None),
            Wake::Frame(raw) => {
                let raw = raw.ok_or(Error::Closed)??;
                let v1 = self.decode(raw)?;
                self.handle(v1).await
            }
            Wake::Joined(j) => self.on_joined(j).await.map(|_| None),
            Wake::KeepAlive => self.keep_alive(false).await.map(|_| None),
            Wake::WifiOn => self.on_wifi_on().await.map(|_| None),
        }
    }

    /// Next payload-level event, or `other`'s output if it finishes first.
    /// Keep-alives and hotspot upgrades are handled here.
    pub async fn next_or<T>(
        &mut self,
        other: impl Future<Output = T>,
    ) -> Result<Result<In, T>, Error> {
        let mut other = std::pin::pin!(other);
        loop {
            match self.wait(other.as_mut()).await {
                Wake::Other(t) => return Ok(Err(t)),
                w => {
                    if let Some(event) = self.react(w).await? {
                        return Ok(Ok(event));
                    }
                }
            }
        }
    }

    /// Handle what already arrived, without waiting (used while we're busy sending).
    pub async fn poll_incoming(&mut self) -> Result<(), Error> {
        if let Some(rx) = &mut self.wifi_on {
            if rx.has_changed().unwrap_or(false) {
                rx.borrow_and_update();
                self.on_wifi_on().await?;
            }
        }
        if let Upgrade::Joining(h, _) = &mut self.upgrade {
            if h.is_finished() {
                let j = h.await;
                self.on_joined(j).await?;
            }
        }
        while let Ok(raw) = self.rx.try_recv() {
            match self.react(Wake::<()>::Frame(Some(raw))).await? {
                Some(In::Bytes(_, data)) => check_cancel(&data)?,
                Some(In::Disconnected) => return Err(Error::Closed),
                _ => {}
            }
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Result<In, Error> {
        Ok(self
            .next_or(pending::<()>())
            .await?
            .unwrap_or_else(|()| unreachable!()))
    }

    async fn handle(&mut self, v1: nc::V1Frame) -> Result<Option<In>, Error> {
        match v1.r#type() {
            FrameType::KeepAlive => {
                if !v1.keep_alive.and_then(|k| k.ack).unwrap_or(false) {
                    self.keep_alive(true).await?;
                }
            }
            FrameType::Disconnection => return Ok(Some(In::Disconnected)),
            FrameType::BandwidthUpgradeNegotiation => {
                self.on_bwu(v1.bandwidth_upgrade_negotiation.unwrap_or_default())
                    .await?
            }
            FrameType::PayloadTransfer => {
                let p = v1
                    .payload_transfer
                    .ok_or(Error::Protocol("missing payload"))?;
                let (h, c) = (
                    p.payload_header.unwrap_or_default(),
                    p.payload_chunk.unwrap_or_default(),
                );
                let (id, offset, last) = (h.id(), c.offset(), c.flags() & 1 == 1);
                let body = c.body.unwrap_or_default();
                match h.r#type() {
                    PayloadType::Bytes => {
                        if h.total_size() as usize > MAX_FRAME {
                            return Err(Error::Protocol("bytes payload too large"));
                        }
                        let buf = self.buffers.entry(id).or_default();
                        if offset != buf.len() as i64 {
                            return Err(Error::Protocol("unexpected chunk offset"));
                        }
                        buf.extend_from_slice(&body);
                        if last {
                            return Ok(Some(In::Bytes(id, self.buffers.remove(&id).unwrap())));
                        }
                    }
                    PayloadType::File => {
                        return Ok(Some(In::File {
                            id,
                            offset,
                            body,
                            last,
                        }))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// Next share-layer frame. A peer CANCEL becomes `Error::Cancelled`.
    pub async fn recv_sharing(&mut self) -> Result<sharing::V1Frame, Error> {
        match self.next().await? {
            In::Bytes(_, data) => parse_sharing(&data),
            In::Disconnected => Err(Error::Closed),
            In::File { .. } => Err(Error::Protocol("unexpected file chunk")),
        }
    }
}

impl Drop for Chan {
    /// A transfer that ended before the hotspot join finished doesn't need the switch any more.
    fn drop(&mut self) {
        if let Upgrade::Joining(h, _) = &self.upgrade {
            h.abort();
        }
    }
}

/// Per-frame protocol trace, on with `QS_DEBUG=1`.
pub fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("QS_DEBUG").is_some())
}

/// Protocol trace on stderr. File data chunks are skipped, Wi-Fi passwords hidden.
fn trace(dir: &str, v1: &nc::V1Frame) {
    if !tracing() {
        return;
    }
    let Some(p) = &v1.payload_transfer else {
        let mut v1 = v1.clone();
        if let Some(info) = v1
            .bandwidth_upgrade_negotiation
            .as_mut()
            .and_then(|b| b.upgrade_path_info.as_mut())
        {
            for pw in [
                info.wifi_hotspot_credentials
                    .as_mut()
                    .map(|c| &mut c.password),
                info.wifi_direct_credentials
                    .as_mut()
                    .map(|c| &mut c.password),
            ]
            .into_iter()
            .flatten()
            {
                *pw = pw.as_ref().map(|_| "***".into());
            }
        }
        eprintln!("qs: {dir} {v1:?}");
        return;
    };
    let (h, c) = (
        p.payload_header.clone().unwrap_or_default(),
        p.payload_chunk.clone().unwrap_or_default(),
    );
    let body = c.body.as_deref().unwrap_or_default();
    match h.r#type() {
        PayloadType::File if c.flags() & 1 == 0 && c.offset() != 0 => {}
        PayloadType::Bytes if !body.is_empty() => match sharing::Frame::decode(body) {
            Ok(f) => eprintln!("qs: {dir} sharing {:?}", f.v1),
            Err(_) => eprintln!("qs: {dir} bytes payload {} ({} bytes)", h.id(), body.len()),
        },
        t => eprintln!(
            "qs: {dir} {t:?} payload {} offset {} flags {}",
            h.id(),
            c.offset(),
            c.flags()
        ),
    }
}

/// `Err(Cancelled)` for a share-layer CANCEL; any other bytes (frames, previews) are ignored.
pub fn check_cancel(data: &[u8]) -> Result<(), Error> {
    match parse_sharing(data) {
        Err(Error::Cancelled) => Err(Error::Cancelled),
        _ => Ok(()),
    }
}

pub fn parse_sharing(data: &[u8]) -> Result<sharing::V1Frame, Error> {
    let v1 = sharing::Frame::decode(data)?
        .v1
        .ok_or(Error::Protocol("missing sharing frame"))?;
    if v1.r#type() == sharing::v1_frame::FrameType::Cancel {
        return Err(Error::Cancelled);
    }
    Ok(v1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::WifiHotspotCredentials;
    use p256::ecdh::EphemeralSecret;

    fn bwu_type(v1: &nc::V1Frame) -> Bwu {
        v1.bandwidth_upgrade_negotiation
            .clone()
            .unwrap_or_default()
            .event_type()
    }

    /// A plain (unencrypted) goodbye ends an encrypted session cleanly; other plain frames are errors.
    #[test]
    fn plaintext_disconnect_accepted() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (a, b) = tokio::io::duplex(1 << 16);
            let (sa, sb) = (
                EphemeralSecret::random(&mut rand::rngs::OsRng),
                EphemeralSecret::random(&mut rand::rngs::OsRng),
            );
            let mut pc = Chan::new(a, Medium::Lan, "PC".into());
            let mut phone = Chan::new(b, Medium::Lan, "PH".into());
            pc.keys = Some(crate::crypto::derive(&sa, &sb.public_key(), b"c", b"s", false).0);
            phone
                .send_raw(&bwu_event(Bwu::LastWriteToPriorChannel).encode_to_vec())
                .await
                .unwrap();
            assert!(pc.next().await.is_err());
            phone
                .send_raw(&disconnection().encode_to_vec())
                .await
                .unwrap();
            assert!(matches!(pc.next().await, Ok(In::Disconnected)));
        });
    }

    /// Turning Wi-Fi on mid-transfer makes a Bluetooth connection ask the phone for its network.
    #[test]
    fn wifi_on_requests_upgrade() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (a, b) = tokio::io::duplex(1 << 16);
            let mut pc = Chan::new(a, Medium::Bluetooth, "JOIN".into());
            let mut phone = Chan::new(b, Medium::Lan, "HOST".into());
            let (tick, rx) = watch::channel(0u32);
            pc.wifi_on = Some(rx);
            pc.allow_upgrade();
            let pc_task = tokio::spawn(async move { pc.next().await });
            tick.send_modify(|n| *n += 1);
            let f = phone.recv().await.unwrap();
            assert_eq!(bwu_type(&f), Bwu::UpgradePathRequest);
            pc_task.abort();
        });
    }

    /// A Bluetooth connection moves to the phone's "hotspot" (localhost TCP here) mid-session,
    /// with the encryption carrying over or dropped as offered, playing the phone's side by hand.
    #[test]
    fn upgrade_swaps_channel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        for plain in [false, true] {
            rt.block_on(async {
                let (a, b) = tokio::io::duplex(1 << 16);
                let (sa, sb) = (
                    EphemeralSecret::random(&mut rand::rngs::OsRng),
                    EphemeralSecret::random(&mut rand::rngs::OsRng),
                );
                let mut pc = Chan::new(a, Medium::Bluetooth, "JOIN".into());
                let mut phone = Chan::new(b, Medium::Lan, "HOST".into());
                pc.keys = Some(crate::crypto::derive(&sa, &sb.public_key(), b"c", b"s", false).0);
                phone.keys = Some(crate::crypto::derive(&sb, &sa.public_key(), b"c", b"s", true).0);
                pc.allow_upgrade();

                let pc_task = tokio::spawn(async move {
                    let got = pc.next().await.unwrap();
                    assert!(matches!(&got, In::Bytes(1, d) if d == b"hello over wifi"));
                    assert_eq!(pc.medium(), Medium::Direct);
                    pc.send_bytes(2, b"back".to_vec()).await.unwrap();
                    pc // keep the connection open until the phone has read
                });

                let hotspot = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = hotspot.local_addr().unwrap().port();
                phone
                    .write_frame(bwu(nc::BandwidthUpgradeNegotiationFrame {
                        event_type: Some(Bwu::UpgradePathAvailable as i32),
                        upgrade_path_info: Some(UpgradePathInfo {
                            medium: Some(upgrade_path_info::Medium::WifiHotspot as i32),
                            wifi_hotspot_credentials: Some(WifiHotspotCredentials {
                                ssid: Some("DIRECT-test".into()),
                                port: Some(port.into()),
                                gateway: Some("127.0.0.1".into()),
                                ..Default::default()
                            }),
                            supports_client_introduction_ack: Some(true),
                            supports_disabling_encryption: Some(plain),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }))
                    .await
                    .unwrap();

                let (mut tcp, _) = hotspot.accept().await.unwrap();
                let intro =
                    nc::OfflineFrame::decode(&*read_frame(&mut tcp).await.unwrap()).unwrap();
                let intro = intro.v1.unwrap();
                assert_eq!(bwu_type(&intro), Bwu::ClientIntroduction);
                let who = intro
                    .bandwidth_upgrade_negotiation
                    .unwrap()
                    .client_introduction
                    .unwrap();
                assert_eq!(who.endpoint_id(), "JOIN");
                assert_eq!(who.supports_disabling_encryption(), plain);
                tcp.write_all(&framed(
                    &bwu_event(Bwu::ClientIntroductionAck).encode_to_vec(),
                ))
                .await
                .unwrap();

                phone
                    .write_frame(bwu_event(Bwu::LastWriteToPriorChannel))
                    .await
                    .unwrap();
                loop {
                    let f = phone.recv().await.unwrap();
                    match bwu_type(&f) {
                        Bwu::LastWriteToPriorChannel => phone
                            .write_frame(bwu_event(Bwu::SafeToClosePriorChannel))
                            .await
                            .unwrap(),
                        Bwu::SafeToClosePriorChannel => break,
                        other => panic!("unexpected {other:?}"),
                    }
                }
                (phone.rx, phone.w) = split(tcp);
                if plain {
                    phone.keys = None;
                }
                phone
                    .send_bytes(1, b"hello over wifi".to_vec())
                    .await
                    .unwrap();
                assert!(matches!(phone.next().await.unwrap(), In::Bytes(2, d) if d == b"back"));
                pc_task.await.unwrap();
            });
        }
    }
}
