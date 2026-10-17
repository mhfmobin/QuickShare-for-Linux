//! Inbound (we are the server/receiver) and outbound (client/sender) share flows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use p256::ecdh::EphemeralSecret;
use prost::Message;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use crate::chan::{
    check_cancel, client_metadata, offline, payload_frame, tracing, Chan, In, Stream,
};
use crate::crypto::{self, NEXT_PROTOCOL};
use crate::proto::{connections as nc, securegcm as ukey, share as sharing};
use crate::{
    bt, Device, DeviceKind, Error, Event, FileInfo, Incoming, Inner, Medium, Outcome, Payload,
    Route, TextKind, TransferId, WifiNetwork, WifiSecurity,
};

use nc::payload_transfer_frame::payload_header::PayloadType;
use sharing::connection_response_frame::Status;
use sharing::v1_frame::FrameType as Share;
use ukey::ukey2_message::Type as Ukey;

const CHUNK: usize = 512 * 1024;
/// Over Bluetooth (~20 KB/s) a 512 KB piece blocks for many seconds, leaving the phone's
/// keep-alives and Wi-Fi offer unanswered; small pieces keep us responsive.
const BT_CHUNK: usize = 32 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

// ---- endpoint info: [flags][16 random][name len][name] ----

pub fn endpoint_info(name: &str) -> Vec<u8> {
    let name = &name.as_bytes()[..name.len().min(255)];
    let mut v = vec![(DeviceKind::Laptop as u8) << 1];
    v.extend(rand::random::<[u8; 16]>());
    v.push(name.len() as u8);
    v.extend_from_slice(name);
    v
}

pub fn parse_endpoint_info(b: &[u8]) -> Option<(String, DeviceKind)> {
    let kind = match (b.first()? & 0x0e) >> 1 {
        1 | 5 => DeviceKind::Phone,
        2 => DeviceKind::Tablet,
        3 => DeviceKind::Laptop,
        _ => DeviceKind::Unknown,
    };
    if b[0] & 0x10 != 0 {
        return None; // hidden device: no plaintext name
    }
    let len = *b.get(17)? as usize;
    let name = String::from_utf8_lossy(b.get(18..18 + len)?).into_owned();
    Some((name, kind))
}

// ---- UKEY2 ----

fn ukey_msg(ty: Ukey, data: Vec<u8>) -> Vec<u8> {
    ukey::Ukey2Message {
        message_type: Some(ty as i32),
        message_data: Some(data),
    }
    .encode_to_vec()
}

fn parse_ukey(raw: &[u8], want: Ukey) -> Result<Vec<u8>, Error> {
    let m = ukey::Ukey2Message::decode(raw)?;
    if tracing() {
        eprintln!("qs: <- UKEY2 {:?}", m.message_type());
    }
    if m.message_type() == Ukey::Alert {
        let alert = ukey::Ukey2Alert::decode(m.message_data())?;
        eprintln!("qs: UKEY2 alert from peer: {alert:?}");
    }
    if m.message_type() != want {
        return Err(Error::Protocol("unexpected UKEY2 message"));
    }
    m.message_data.ok_or(Error::Protocol("empty UKEY2 message"))
}

async fn ukey2_server(ch: &mut Chan) -> Result<(crypto::Keys, String), Error> {
    let client_init_raw = ch.recv_raw().await?;
    let init = ukey::Ukey2ClientInit::decode(&*parse_ukey(&client_init_raw, Ukey::ClientInit)?)?;
    if init.version() != 1 || init.random().len() != 32 || init.next_protocol() != NEXT_PROTOCOL {
        return Err(Error::Protocol("unsupported UKEY2 client init"));
    }
    let commitment = init
        .cipher_commitments
        .iter()
        .find(|c| c.handshake_cipher() == ukey::Ukey2HandshakeCipher::P256Sha512)
        .and_then(|c| c.commitment.clone())
        .ok_or(Error::Protocol("no P256 cipher offered"))?;

    let secret = EphemeralSecret::random(&mut rand::rngs::OsRng);
    let server_init_raw = ukey_msg(
        Ukey::ServerInit,
        ukey::Ukey2ServerInit {
            version: Some(1),
            random: Some(rand::random::<[u8; 32]>().to_vec()),
            handshake_cipher: Some(ukey::Ukey2HandshakeCipher::P256Sha512 as i32),
            public_key: Some(crypto::encode_public_key(&secret.public_key())),
        }
        .encode_to_vec(),
    );
    ch.send_raw(&server_init_raw).await?;

    let finish_raw = ch.recv_raw().await?;
    if crypto::sha512(&finish_raw) != commitment {
        return Err(Error::Protocol("UKEY2 commitment mismatch"));
    }
    let finish = ukey::Ukey2ClientFinished::decode(&*parse_ukey(&finish_raw, Ukey::ClientFinish)?)?;
    let peer = crypto::decode_public_key(finish.public_key())?;
    Ok(crypto::derive(
        &secret,
        &peer,
        &client_init_raw,
        &server_init_raw,
        true,
    ))
}

async fn ukey2_client(ch: &mut Chan) -> Result<(crypto::Keys, String), Error> {
    let secret = EphemeralSecret::random(&mut rand::rngs::OsRng);
    let finish_raw = ukey_msg(
        Ukey::ClientFinish,
        ukey::Ukey2ClientFinished {
            public_key: Some(crypto::encode_public_key(&secret.public_key())),
        }
        .encode_to_vec(),
    );
    let client_init_raw = ukey_msg(
        Ukey::ClientInit,
        ukey::Ukey2ClientInit {
            version: Some(1),
            random: Some(rand::random::<[u8; 32]>().to_vec()),
            cipher_commitments: vec![ukey::ukey2_client_init::CipherCommitment {
                handshake_cipher: Some(ukey::Ukey2HandshakeCipher::P256Sha512 as i32),
                commitment: Some(crypto::sha512(&finish_raw)),
            }],
            next_protocol: Some(NEXT_PROTOCOL.into()),
        }
        .encode_to_vec(),
    );
    ch.send_raw(&client_init_raw).await?;

    let server_init_raw = ch.recv_raw().await?;
    let init = ukey::Ukey2ServerInit::decode(&*parse_ukey(&server_init_raw, Ukey::ServerInit)?)?;
    if init.version() != 1 || init.handshake_cipher() != ukey::Ukey2HandshakeCipher::P256Sha512 {
        return Err(Error::Protocol("unsupported UKEY2 server init"));
    }
    let peer = crypto::decode_public_key(init.public_key())?;
    ch.send_raw(&finish_raw).await?;
    Ok(crypto::derive(
        &secret,
        &peer,
        &client_init_raw,
        &server_init_raw,
        false,
    ))
}

// ---- shared handshake pieces ----

#[allow(deprecated)] // Android still reads the old `status` field
fn connection_response() -> nc::OfflineFrame {
    offline(nc::V1Frame {
        r#type: Some(nc::v1_frame::FrameType::ConnectionResponse as i32),
        connection_response: Some(nc::ConnectionResponseFrame {
            status: Some(0),
            response: Some(nc::connection_response_frame::ResponseStatus::Accept as i32),
            os_info: Some(nc::OsInfo {
                r#type: Some(nc::os_info::OsType::Linux as i32),
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn share(ty: Share) -> sharing::V1Frame {
    sharing::V1Frame {
        r#type: Some(ty as i32),
        ..Default::default()
    }
}

fn response(status: Status) -> sharing::V1Frame {
    sharing::V1Frame {
        connection_response: Some(sharing::ConnectionResponseFrame {
            status: Some(status as i32),
            ..Default::default()
        }),
        ..share(Share::Response)
    }
}

async fn expect(ch: &mut Chan, ty: Share) -> Result<sharing::V1Frame, Error> {
    loop {
        let f = ch.recv_sharing().await?;
        if f.r#type() == ty {
            return Ok(f);
        }
        // Android sometimes sends its RESPONSE early (seen before INTRODUCTION); skip it
        if f.r#type() != Share::Response {
            return Err(Error::Protocol("unexpected share frame"));
        }
    }
}

/// The "paired key" exchange is for Google-account contacts; random bytes are accepted
/// from non-contacts. Returns after both sides exchanged encryption + result frames.
async fn paired_key_dance(ch: &mut Chan) -> Result<(), Error> {
    ch.send_sharing(sharing::V1Frame {
        paired_key_encryption: Some(sharing::PairedKeyEncryptionFrame {
            secret_id_hash: Some(rand::random::<[u8; 6]>().to_vec()),
            signed_data: Some((0..72).map(|_| rand::random::<u8>()).collect()),
            ..Default::default()
        }),
        ..share(Share::PairedKeyEncryption)
    })
    .await?;
    expect(ch, Share::PairedKeyEncryption).await?;
    ch.send_sharing(sharing::V1Frame {
        paired_key_result: Some(sharing::PairedKeyResultFrame {
            status: Some(sharing::paired_key_result_frame::Status::Unable as i32),
            ..Default::default()
        }),
        ..share(Share::PairedKeyResult)
    })
    .await?;
    expect(ch, Share::PairedKeyResult).await?;
    Ok(())
}

/// Deletes partially received files, and the folders made for them, unless disarmed.
/// Also runs when the task is aborted.
#[derive(Default)]
struct Partial {
    files: Vec<PathBuf>,
    /// Top-level folders we created (always fresh), so they only ever hold our own files.
    dirs: Vec<PathBuf>,
}
impl Drop for Partial {
    fn drop(&mut self) {
        for p in &self.files {
            let _ = std::fs::remove_file(p);
        }
        for d in &self.dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Never trust a peer-supplied name as a path.
pub fn safe_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .replace('\0', "");
    let base = base.trim().trim_start_matches('.');
    if base.is_empty() {
        "file".into()
    } else {
        base.chars().take(200).collect()
    }
}

/// A peer-supplied folder path, made safe: "/a/../b\\c" -> ["a", "b", "c"].
pub fn safe_folder(path: &str) -> Vec<String> {
    path.split(['/', '\\'])
        .filter(|p| !p.trim().trim_start_matches('.').is_empty())
        .map(safe_name)
        .collect()
}

/// "name", then "name (1)", "name (2)"… keeping a file's extension at the end.
fn numbered(name: &str, i: u32, keep_ext: bool) -> String {
    match name.rsplit_once('.') {
        _ if i == 0 => name.to_string(),
        Some((stem, ext)) if keep_ext && !stem.is_empty() => format!("{stem} ({i}).{ext}"),
        _ => format!("{name} ({i})"),
    }
}

async fn create_unique(dir: &Path, name: &str) -> std::io::Result<(PathBuf, File)> {
    for i in 0.. {
        let path = dir.join(numbered(name, i, true));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

async fn create_unique_dir(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    for i in 0.. {
        let path = dir.join(numbered(name, i, false));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

// ---- inbound ----

/// A new connection: tell the UI what it runs over now and on every switch.
fn open(inner: &Inner, id: TransferId, stream: impl Stream, medium: Medium) -> Chan {
    let mut ch = Chan::new(
        stream,
        medium,
        String::from_utf8_lossy(&inner.endpoint_id).into(),
    );
    let events = inner.events.clone();
    ch.on_medium = Some(Box::new(move |medium| {
        let _ = events.send(Event::Medium { id, medium });
    }));
    ch.wifi_on = Some(inner.wifi_on.subscribe());
    inner.emit(Event::Medium { id, medium });
    ch
}

pub async fn inbound(
    inner: &Inner,
    id: TransferId,
    stream: impl Stream,
    medium: Medium,
) -> Result<Outcome, Error> {
    let mut ch = open(inner, id, stream, medium);
    let req = ch
        .recv()
        .await?
        .connection_request
        .ok_or(Error::Protocol("expected connection request"))?;
    let (from, kind) =
        parse_endpoint_info(req.endpoint_info()).ok_or(Error::Protocol("bad endpoint info"))?;

    let (keys, pin) = ukey2_server(&mut ch).await?;
    if ch.recv().await?.connection_response.is_none() {
        return Err(Error::Protocol("expected connection response"));
    }
    ch.send(connection_response()).await?;
    ch.keys = Some(keys);
    paired_key_dance(&mut ch).await?;

    let intro = expect(&mut ch, Share::Introduction)
        .await?
        .introduction
        .unwrap_or_default();
    let files: Vec<FileInfo> = intro
        .file_metadata
        .iter()
        .map(|f| FileInfo {
            name: safe_name(f.name()),
            folder: safe_folder(f.parent_folder()),
            size: f.size().max(0) as u64,
            mime: f.mime_type().into(),
        })
        .collect();
    let unsupported = !intro.app_metadata.is_empty() || !intro.stream_metadata.is_empty();
    let empty = files.is_empty()
        && intro.text_metadata.is_empty()
        && intro.wifi_credentials_metadata.is_empty();
    if unsupported || empty {
        ch.send_sharing(response(Status::UnsupportedAttachmentType))
            .await?;
        ch.disconnect().await;
        return Ok(Outcome::Failed("Unsupported content".into()));
    }

    let (reply, mut consent) = oneshot::channel();
    inner.emit(Event::Incoming(Incoming {
        id,
        from,
        kind,
        pin,
        medium: ch.medium(),
        files: files.clone(),
        texts: intro
            .text_metadata
            .iter()
            .map(|t| (t.text_title().to_string(), TextKind::from(t.r#type())))
            .collect(),
        wifi: intro
            .wifi_credentials_metadata
            .iter()
            .map(|w| w.ssid().to_string())
            .collect(),
        reply,
    }));
    let accepted = loop {
        match ch.next_or(&mut consent).await? {
            Err(answer) => break answer.unwrap_or(false),
            // Android sends extra frames here (e.g. its own RESPONSE); only a CANCEL ends the wait
            Ok(In::Bytes(_, data)) => check_cancel(&data)?,
            Ok(_) => return Ok(Outcome::Cancelled), // sender left while we were asking
        }
    };
    if !accepted {
        ch.send_sharing(response(Status::Reject)).await?;
        ch.disconnect().await;
        return Ok(Outcome::Rejected);
    }
    // Accepting over Bluetooth includes agreeing to the faster Wi-Fi switch.
    if ch.medium() == Medium::Bluetooth {
        ch.allow_upgrade();
        if crate::hotspot::available().await {
            ch.request_upgrade().await?;
        } else if crate::hotspot::radio_off().await {
            inner.emit(Event::WifiOff { id }); // asked once the user turns it on
        }
    }

    let dir = inner.settings.lock().unwrap().download_dir.clone();
    tokio::fs::create_dir_all(&dir).await?;
    let mut partial = Partial::default();
    let mut saved = vec![]; // what the user sees in the download folder: top-level files and folders
    let mut tops: HashMap<String, PathBuf> = HashMap::new(); // sent folder name -> folder we made
                                                             // payload id -> (file, written, expected size)
    let mut open = HashMap::new();
    for (meta, info) in intro.file_metadata.iter().zip(&files) {
        let mut target = dir.clone();
        if let Some((top, rest)) = info.folder.split_first() {
            if !tops.contains_key(top) {
                let made = create_unique_dir(&dir, top).await?;
                partial.dirs.push(made.clone());
                saved.push(made.clone());
                tops.insert(top.clone(), made);
            }
            target = rest.iter().fold(tops[top].clone(), |d, p| d.join(p));
            tokio::fs::create_dir_all(&target).await?;
        }
        let (path, file) = create_unique(&target, &info.name).await?;
        if info.folder.is_empty() {
            saved.push(path.clone());
        }
        partial.files.push(path);
        open.insert(meta.payload_id(), (file, 0u64, info.size));
    }
    let text_slot: HashMap<i64, usize> = intro
        .text_metadata
        .iter()
        .enumerate()
        .map(|(i, t)| (t.payload_id(), i))
        .collect();
    let mut texts: Vec<Option<String>> = vec![None; text_slot.len()];
    let wifi_slot: HashMap<i64, usize> = intro
        .wifi_credentials_metadata
        .iter()
        .enumerate()
        .map(|(i, w)| (w.payload_id(), i))
        .collect();
    let mut wifi: Vec<Option<WifiNetwork>> = vec![None; wifi_slot.len()];
    ch.send_sharing(response(Status::Accept)).await?;

    let total: u64 = files.iter().map(|f| f.size).sum();
    let (mut done, mut last_report) = (0u64, Instant::now());
    while !open.is_empty() || texts.iter().any(Option::is_none) || wifi.iter().any(Option::is_none)
    {
        match ch.next().await? {
            In::File {
                id: pid,
                offset,
                body,
                last,
            } => {
                // Payloads we didn't ask for (e.g. previews) are skipped.
                let Some((file, written, size)) = open.get_mut(&pid) else {
                    continue;
                };
                if offset as u64 != *written || *written + body.len() as u64 > *size {
                    return Err(Error::Protocol("bad file chunk"));
                }
                file.write_all(&body).await?;
                *written += body.len() as u64;
                done += body.len() as u64;
                if last {
                    if *written != *size {
                        return Err(Error::Protocol("file truncated"));
                    }
                    file.flush().await?;
                    open.remove(&pid);
                }
                if last_report.elapsed() >= PROGRESS_EVERY {
                    last_report = Instant::now();
                    inner.emit(Event::Progress { id, done, total });
                }
            }
            In::Bytes(pid, data) => match (text_slot.get(&pid), wifi_slot.get(&pid)) {
                (Some(&i), _) => texts[i] = Some(String::from_utf8_lossy(&data).into()),
                (_, Some(&i)) => {
                    wifi[i] = Some(wifi_network(&intro.wifi_credentials_metadata[i], &data)?)
                }
                _ => check_cancel(&data)?,
            },
            In::Disconnected => return Err(Error::Closed),
        }
    }
    ch.disconnect().await;
    partial.files.clear(); // keep everything
    partial.dirs.clear();
    Ok(Outcome::Received {
        files: saved,
        texts: texts.into_iter().flatten().collect(),
        wifi: wifi.into_iter().flatten().collect(),
    })
}

/// A shared network: name and security from the introduction, password from its BYTES payload.
pub(crate) fn wifi_network(
    meta: &sharing::WifiCredentialsMetadata,
    data: &[u8],
) -> Result<WifiNetwork, Error> {
    use sharing::wifi_credentials_metadata::SecurityType as S;
    let creds = sharing::WifiCredentials::decode(data)?;
    Ok(WifiNetwork {
        ssid: meta.ssid().into(),
        password: creds.password().into(),
        security: match meta.security_type() {
            S::Open => WifiSecurity::Open,
            S::Wep => WifiSecurity::Wep,
            S::Sae => WifiSecurity::Sae,
            _ => WifiSecurity::Wpa,
        },
        hidden: creds.hidden_ssid(),
    })
}

// ---- outbound ----

fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        "apk" => "application/vnd.android.package-archive",
        _ => "application/octet-stream",
    }
}

fn file_type(mime: &str) -> sharing::file_metadata::Type {
    use sharing::file_metadata::Type::*;
    match mime.split('/').next() {
        Some("image") => Image,
        Some("video") => Video,
        Some("audio") => Audio,
        _ if mime.contains("android") => AndroidApp,
        _ => Unknown,
    }
}

/// Files to send, each with the folder path it goes in ("" at the top level).
/// Folders are walked recursively; symlinks inside them are skipped.
fn expand(paths: &[PathBuf]) -> std::io::Result<Vec<(PathBuf, String)>> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) -> std::io::Result<()> {
        let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let ty = e.file_type()?;
            if ty.is_dir() {
                walk(&e.path(), base, out)?;
            } else if ty.is_file() {
                let rel = dir.strip_prefix(base).unwrap_or(dir);
                out.push((e.path(), rel.to_string_lossy().into_owned()));
            }
        }
        Ok(())
    }
    let mut out = vec![];
    for p in paths {
        if p.is_dir() {
            walk(p, p.parent().unwrap_or(Path::new("")), &mut out)?;
        } else {
            out.push((p.clone(), String::new()));
        }
    }
    Ok(out)
}

pub async fn outbound(
    inner: &Inner,
    id: TransferId,
    device: Device,
    payload: Payload,
) -> Result<Outcome, Error> {
    eprintln!("qs: connecting to {} via {:?}", device.name, device.route);
    use nc::connection_request_frame::Medium as M;
    let (mut ch, mediums, medium_metadata) = match device.route {
        Route::Lan(addr) => {
            let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
                .await
                .map_err(|_| Error::Protocol("device did not answer"))??;
            let _ = stream.set_nodelay(true);
            (
                open(inner, id, stream, Medium::Lan),
                vec![M::WifiLan as i32],
                None,
            )
        }
        Route::Bluetooth(addr) => {
            // ponytail: fixed pause so the LE adverts/scan (stopped via Inner::busy) are down before
            // we page the phone; have bt::run confirm instead if 1 s turns out too short.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let stream = bt::connect(addr).await?;
            let mut ch = open(inner, id, stream, Medium::Bluetooth);
            ch.allow_upgrade(); // the user chose a Bluetooth-only device, and the send screen said so
            let mediums = [M::Bluetooth, M::WifiHotspot, M::WifiDirect];
            (
                ch,
                mediums.map(|m| m as i32).to_vec(),
                Some(client_metadata()),
            )
        }
    };
    let name = inner.settings.lock().unwrap().name.clone();
    ch.send(offline(nc::V1Frame {
        r#type: Some(nc::v1_frame::FrameType::ConnectionRequest as i32),
        connection_request: Some(nc::ConnectionRequestFrame {
            endpoint_id: Some(String::from_utf8_lossy(&inner.endpoint_id).into()),
            endpoint_name: Some(name.clone()),
            endpoint_info: Some(endpoint_info(&name)),
            mediums,
            medium_metadata,
            ..Default::default()
        }),
        ..Default::default()
    }))
    .await?;

    let (keys, pin) = ukey2_client(&mut ch).await?;
    ch.send(connection_response()).await?;
    let resp = ch
        .recv()
        .await?
        .connection_response
        .ok_or(Error::Protocol("expected connection response"))?;
    if resp.response() != nc::connection_response_frame::ResponseStatus::Accept {
        return Ok(Outcome::Rejected);
    }
    ch.keys = Some(keys);
    inner.emit(Event::Connected { id, pin });
    paired_key_dance(&mut ch).await?;

    let mut intro = sharing::IntroductionFrame::default();
    let mut text_payloads = vec![];
    for text in payload.texts {
        let pid: i64 = rand::random();
        intro.text_metadata.push(sharing::TextMetadata {
            text_title: Some(text.chars().take(40).collect()),
            r#type: Some(
                if text.starts_with("http://") || text.starts_with("https://") {
                    sharing::text_metadata::Type::Url
                } else {
                    sharing::text_metadata::Type::Text
                } as i32,
            ),
            payload_id: Some(pid),
            size: Some(text.len() as i64),
            id: Some(rand::random()),
            ..Default::default()
        });
        text_payloads.push((pid, text));
    }
    let mut sources = vec![];
    for (path, folder) in expand(&payload.files)? {
        let file = File::open(&path).await?;
        let size = file.metadata().await?.len();
        let mime = mime_for(&path);
        let pid: i64 = rand::random();
        intro.file_metadata.push(sharing::FileMetadata {
            name: Some(safe_name(
                &path.file_name().unwrap_or_default().to_string_lossy(),
            )),
            r#type: Some(file_type(mime) as i32),
            payload_id: Some(pid),
            size: Some(size as i64),
            mime_type: Some(mime.into()),
            id: Some(rand::random()),
            parent_folder: (!folder.is_empty()).then_some(folder),
            ..Default::default()
        });
        sources.push((pid, file, size));
    }
    ch.send_sharing(sharing::V1Frame {
        introduction: Some(intro),
        ..share(Share::Introduction)
    })
    .await?;

    let status = expect(&mut ch, Share::Response)
        .await?
        .connection_response
        .unwrap_or_default()
        .status();
    match status {
        Status::Accept => {}
        Status::NotEnoughSpace => {
            return Ok(Outcome::Failed("Not enough space on the device".into()))
        }
        Status::UnsupportedAttachmentType => {
            return Ok(Outcome::Failed("The device can't receive this type".into()))
        }
        Status::TimedOut => return Ok(Outcome::Failed("Timed out".into())),
        _ => return Ok(Outcome::Rejected),
    }
    inner.emit(Event::Accepted { id });
    if ch.medium() == Medium::Bluetooth && crate::hotspot::radio_off().await {
        inner.emit(Event::WifiOff { id });
    }

    for (pid, text) in text_payloads {
        ch.send_bytes(pid, text.into_bytes()).await?;
    }
    let total: u64 = sources.iter().map(|s| s.2).sum();
    let (mut done, mut last_report) = (0u64, Instant::now());
    let mut buf = vec![0; CHUNK];
    for (pid, mut file, size) in sources {
        let mut offset = 0u64;
        loop {
            ch.poll_incoming().await?; // the phone may cancel while we send
            let piece = if ch.medium() == Medium::Bluetooth {
                BT_CHUNK
            } else {
                CHUNK
            };
            let n = file.read(&mut buf[..piece]).await?;
            let last = n == 0 || offset + n as u64 >= size;
            if n > 0 {
                ch.send(payload_frame(
                    pid,
                    PayloadType::File,
                    size as i64,
                    offset as i64,
                    buf[..n].to_vec(),
                    false,
                ))
                .await?;
                offset += n as u64;
                done += n as u64;
            }
            if last {
                if offset != size {
                    return Err(Error::Protocol("file changed while sending"));
                }
                ch.send(payload_frame(
                    pid,
                    PayloadType::File,
                    size as i64,
                    offset as i64,
                    vec![],
                    true,
                ))
                .await?;
                break;
            }
            if last_report.elapsed() >= PROGRESS_EVERY {
                last_report = Instant::now();
                inner.emit(Event::Progress { id, done, total });
            }
        }
    }
    inner.emit(Event::Progress { id, done, total });
    // The receiver hangs up once everything is stored; hanging up first makes Android drop the files.
    let finished = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match ch.next().await? {
                In::Disconnected => return Ok(()),
                In::Bytes(_, data) => check_cancel(&data)?,
                In::File { .. } => {}
            }
        }
    })
    .await;
    match finished {
        Ok(Err(Error::Closed)) | Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(Error::Protocol("the device didn't confirm the transfer")),
    }
    ch.disconnect().await;
    Ok(Outcome::Sent)
}
