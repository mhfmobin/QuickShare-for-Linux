<div align="center">

<img src="data/io.github.mhfmobin.QuickShareLinux.svg" width="96" alt="">

# QuickShare for Linux

**Send and receive files and links between your Linux PC and nearby Android phones, using the Quick Share already built into the phone.**

A native GNOME app (GTK4 + libadwaita) with its own Quick Share protocol implementation, written in Rust.

[Download](https://github.com/mhfmobin/QuickShare-for-Linux/releases/latest) · [Report a bug](https://github.com/mhfmobin/QuickShare-for-Linux/issues)

</div>

## Features

- **Receive** files, folders, links and text from any Android phone's share sheet. The PIN is shown on both sides so you can check you're accepting the right phone.
- **Send** by dropping files on the window, pressing <kbd>Ctrl</kbd>+<kbd>O</kbd>, pasting a link or text with <kbd>Ctrl</kbd>+<kbd>V</kbd>, or right-clicking files in Files (Nautilus) and choosing **Send via QuickShare**.
- **Visibility** like on the phone: everyone, temporarily, or hidden.
- **Trusted devices** whose shares are accepted without asking.
- **Background mode**: keep running after the window is closed, start at login, and handle shares from notifications.
- **No shared Wi‑Fi needed**: with Bluetooth on, the phone finds the PC over Bluetooth LE and the transfer moves to the phone's own Wi‑Fi Direct link for speed *(experimental)*.

## Install

Download the `.deb` from the [latest release](https://github.com/mhfmobin/QuickShare-for-Linux/releases/latest) and install it:

```sh
sudo apt install ./QuickShare-for-linux-amd64-*.deb
```

It runs on **Debian 13+**, **Ubuntu 25.04+** and distros based on them (it needs libadwaita 1.6+).
The right-click **Send via QuickShare** item in Files needs `python3-nautilus` (apt installs it automatically)
and shows up once Files restarts: run `nautilus -q` or log out and back in.

## Usage

**Receive from a phone:** on the phone, share something → **Quick Share** → pick this PC → check the PIN matches → **Accept**.
Files land in `~/Downloads`; you can change this in Preferences.

**Send to a phone:** on the phone, swipe down, tap **Quick Share** and set visibility to **Everyone**.
Then drop files on the window and click the phone when it appears.

### Requirements and troubleshooting

| | |
|---|---|
| **Same network** | Phone and PC should be on the same Wi‑Fi. Guest networks that isolate devices from each other won't work. |
| **Bluetooth** | Needed for sending: phones only announce themselves after hearing the PC over Bluetooth. Receiving works with Wi‑Fi alone. |
| **Firewall** | Allow TCP port 47130 (changeable in Preferences) and UDP 5353, e.g. `sudo ufw allow 47130/tcp && sudo ufw allow 5353/udp`. The app shows the exact command when it sees a firewall. |
| **VPN** | Some VPNs block local traffic. For NordVPN, run `nordvpn set lan-discovery on`, or disconnect while sharing. |

Still stuck? Run it with a protocol trace and attach the log to an [issue](https://github.com/mhfmobin/QuickShare-for-Linux/issues):

```sh
QS_DEBUG=1 quickshare-linux 2>&1 | tee qs.log
```

## Build from source

You need Rust (stable) and the GTK4, libadwaita and D-Bus development headers:

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libdbus-1-dev
cargo build --release -p quickshare-linux
./target/release/quickshare-linux
```

To build a `.deb`: `cargo install cargo-deb && cargo deb -p quickshare-linux`.

If you'd rather not install anything on your machine, `./dev.sh` runs any command in a Docker container instead:

```sh
./dev.sh cargo test -p qs-core -- --include-ignored
./dev.sh cargo build --release -p quickshare-linux
```

### Project layout

| Path | What's there |
|---|---|
| `core/` | Quick Share protocol: UKEY2 handshake, encrypted channel, payloads, mDNS and Bluetooth discovery, Wi‑Fi Direct upgrade. No GTK. |
| `app/` | GTK4/libadwaita user interface. |
| `nautilus/` | "Send via QuickShare" right-click item for Files. |
| `data/` | Desktop entry, icon and bundled fonts. |
| `build-aux/` | Flatpak manifest (work in progress). |

### Releasing

Push a tag like `v1.2.3`. GitHub Actions builds the `.deb` on Debian 13 and attaches it to a new GitHub release.

## Not yet supported

- Receiving shared Wi‑Fi networks (passwords).
- Flatpak / Flathub.

## License

[GPL-3.0-or-later](LICENSE). The protocol definitions in `core/proto/` come from [google/nearby](https://github.com/google/nearby) and are Apache-2.0. The bundled fonts are under the [SIL Open Font License](data/fonts/OFL.txt).

QuickShare for Linux is an independent project. It is not affiliated with or endorsed by Google or Samsung. Quick Share is a trademark of its respective owners.
