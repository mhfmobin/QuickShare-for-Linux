//! Joining the phone's temporary Wi-Fi network (Nearby's WIFI_HOTSPOT / WIFI_DIRECT upgrade; a Wi-Fi
//! Direct group takes legacy clients with SSID + passphrase, so both are joined the same way).
//! The PC leaves its current Wi-Fi for the transfer and goes back afterwards.
// ponytail: drives NetworkManager through nmcli; use its D-Bus API if a Flatpak build needs it.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::proto::connections::bandwidth_upgrade_negotiation_frame::{
    upgrade_path_info::Medium as M, UpgradePathInfo,
};
use crate::Error;

/// Connection profiles we create are named like this, so leftovers can be cleaned up.
const PREFIX: &str = "quickshare-direct-";

fn fail(msg: impl Into<String>) -> Error {
    Error::Io(std::io::Error::other(msg.into()))
}

/// While alive, we're on the phone's network. Dropping it deletes the temporary profile
/// and brings the previous Wi-Fi connection back.
pub struct Wifi {
    profile: String,
    previous: Option<String>,
}

impl Drop for Wifi {
    fn drop(&mut self) {
        if self.profile.is_empty() {
            return; // test stand-in
        }
        let (profile, previous) = (self.profile.clone(), self.previous.take());
        std::thread::spawn(move || {
            let run = |args: &[&str]| std::process::Command::new("nmcli").args(args).output();
            let _ = run(&["connection", "delete", &profile]);
            if let Some(prev) = previous {
                eprintln!("qs: reconnecting to {prev}");
                let _ = run(&["--wait", "30", "connection", "up", &prev]);
            }
        });
    }
}

async fn nmcli(args: &[&str]) -> Result<String, Error> {
    // English messages, so the error text checks below hold under any locale
    let out = tokio::process::Command::new("nmcli")
        .env_remove("LC_ALL")
        .env("LC_MESSAGES", "C")
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(fail(format!(
            "nmcli: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn wifi_interface() -> Option<String> {
    std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .find(|e| e.path().join("wireless").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

/// NetworkManager band + channel for a frequency in MHz, so it looks only there for the phone's network.
fn band_channel(mhz: i32) -> Option<(&'static str, String)> {
    match mhz {
        2484 => Some(("bg", "14".into())),
        2412..=2472 => Some(("bg", ((mhz - 2407) / 5).to_string())),
        5160..=5885 => Some(("a", ((mhz - 5000) / 5).to_string())),
        _ => None, // unknown, -1, or a band number instead of a frequency
    }
}

/// The connection active on `ifname`, from `nmcli -t` output ("name:device", with ':' escaped as "\:").
fn active_on(terse: &str, ifname: &str) -> Option<String> {
    terse.lines().find_map(|l| {
        let (name, dev) = l.rsplit_once(':')?;
        (dev == ifname).then(|| name.replace("\\:", ":"))
    })
}

/// Join the network the phone offers and connect to its socket there.
pub async fn join(info: UpgradePathInfo) -> Result<(TcpStream, Wifi), Error> {
    let (ssid, password, port, gateway, freq) = match info.medium() {
        M::WifiHotspot => {
            let c = info.wifi_hotspot_credentials.unwrap_or_default();
            (
                c.ssid().to_owned(),
                c.password().to_owned(),
                c.port(),
                c.gateway().to_owned(),
                c.frequency(),
            )
        }
        M::WifiDirect => {
            let c = info.wifi_direct_credentials.unwrap_or_default();
            (
                c.ssid().to_owned(),
                c.password().to_owned(),
                c.port(),
                c.gateway().to_owned(),
                c.frequency(),
            )
        }
        _ => return Err(Error::Protocol("unsupported upgrade medium")),
    };
    #[cfg(test)]
    if gateway.starts_with("127.") {
        let stream = TcpStream::connect(format!("{gateway}:{port}")).await?;
        let wifi = Wifi {
            profile: String::new(),
            previous: None,
        };
        return Ok((stream, wifi));
    }
    let ifname = wifi_interface().ok_or_else(|| fail("no Wi-Fi adapter"))?;
    let previous = active_on(
        &nmcli(&["-t", "-f", "NAME,DEVICE", "connection", "show", "--active"]).await?,
        &ifname,
    );
    let profile = format!("{PREFIX}{:04x}", rand::random::<u16>());
    // ponytail: the passphrase is briefly visible in the process list; it's single-use and dies with the hotspot.
    let mut add = vec![
        "connection",
        "add",
        "type",
        "wifi",
        "ifname",
        &ifname,
        "con-name",
        &profile,
        "ssid",
        &ssid,
        "autoconnect",
        "no",
        "802-11-wireless.hidden",
        "yes",
        "ipv6.method",
        "disabled",
    ];
    if !password.is_empty() {
        add.extend(["wifi-sec.key-mgmt", "wpa-psk", "wifi-sec.psk", &password]);
    }
    let hint = band_channel(freq);
    if let Some((band, channel)) = &hint {
        add.extend([
            "802-11-wireless.band",
            band,
            "802-11-wireless.channel",
            channel,
        ]);
    }
    nmcli(&add).await?;
    let wifi = Wifi {
        profile: profile.clone(),
        previous,
    };
    if let Err(e) = nmcli(&["--wait", "30", "connection", "up", &profile]).await {
        // WPA3 can't help when the network wasn't seen at all
        if password.is_empty() || e.to_string().contains("could not be found") {
            return Err(e);
        }
        // Newer Android hotspots can be WPA3-only.
        nmcli(&["connection", "modify", &profile, "wifi-sec.key-mgmt", "sae"]).await?;
        nmcli(&["--wait", "30", "connection", "up", &profile]).await?;
    }

    let gateway = match gateway.as_str() {
        "" | "0.0.0.0" => nmcli(&["-g", "IP4.GATEWAY", "device", "show", &ifname])
            .await?
            .trim()
            .to_owned(),
        _ => gateway,
    };
    let addr: SocketAddr = format!("{gateway}:{port}")
        .parse()
        .map_err(|_| fail(format!("bad hotspot address {gateway}:{port}")))?;
    // The phone's socket can lag a moment behind its network coming up.
    for attempt in 0.. {
        match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok((stream, wifi)),
            _ if attempt < 3 => tokio::time::sleep(Duration::from_secs(1)).await,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(fail("the phone's hotspot didn't answer")),
        }
    }
    unreachable!()
}

/// Whether we can join a phone's network right now: a Wi-Fi adapter with its radio on.
pub async fn available() -> bool {
    let (ifname, radio) = (wifi_interface(), nmcli(&["radio", "wifi"]).await);
    let ok = ifname.is_some() && radio.as_ref().is_ok_and(|s| s.trim() == "enabled");
    if !ok {
        eprintln!("qs: not asking for the Wi-Fi switch: adapter {ifname:?}, radio {radio:?}");
    }
    ok
}

/// A Wi-Fi adapter exists but its radio is switched off (so the user could turn it on).
pub async fn radio_off() -> bool {
    wifi_interface().is_some()
        && nmcli(&["radio", "wifi"])
            .await
            .is_ok_and(|s| s.trim() == "disabled")
}

/// Switch the Wi-Fi radio on and wait (up to 15 s) until the adapter can join networks.
pub async fn turn_on() -> Result<(), Error> {
    let ifname = wifi_interface().ok_or_else(|| fail("no Wi-Fi adapter"))?;
    nmcli(&["radio", "wifi", "on"]).await?;
    for _ in 0..30 {
        // "20 (unavailable)" until the radio is up; "10 (unmanaged)" never becomes usable
        let state = nmcli(&["-g", "GENERAL.STATE", "device", "show", &ifname]).await?;
        if !state.starts_with("20") && !state.starts_with("10") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(fail("Wi-Fi didn't come up"))
}

/// Delete profiles a crash left behind, so the PC never auto-joins an old phone hotspot.
pub async fn cleanup() {
    let Ok(out) = nmcli(&["-t", "-f", "NAME", "connection", "show"]).await else {
        return;
    };
    for name in out.lines().filter(|n| n.starts_with(PREFIX)) {
        let _ = nmcli(&["connection", "delete", name]).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn active_connection_parsing() {
        let out = "Home\\: 5G:wlp0s20f3\nlo:lo\ndocker0:docker0\n";
        assert_eq!(
            super::active_on(out, "wlp0s20f3").as_deref(),
            Some("Home: 5G")
        );
        assert_eq!(super::active_on(out, "eth0"), None);
    }

    #[test]
    fn frequency_to_channel() {
        let ch = |f| super::band_channel(f).map(|(b, c)| format!("{b}{c}"));
        assert_eq!(ch(5765).as_deref(), Some("a153"));
        assert_eq!(ch(5180).as_deref(), Some("a36"));
        assert_eq!(ch(2412).as_deref(), Some("bg1"));
        assert_eq!(ch(2484).as_deref(), Some("bg14"));
        assert_eq!(ch(-1), None);
        assert_eq!(ch(5), None);
    }
}
