# Spritz

[![ci](https://github.com/dfallman/spritz/actions/workflows/ci.yml/badge.svg)](https://github.com/dfallman/spritz/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`spritz` is a nano DLNA media server. Run it in a folder and that folder's video and audio becomes instantly available from TVs, phones, speakers, and other media players on your local network.

```
cd /mnt/nas/movies
spritz
```
or, alternatively:
```
spritz /mnt/nas/movies /mnt/nas/music 
```

Expected output:
```
Indexed XX media file(s)
Serving on http://192.168.XXX.XXX:8080/spritz
DLNA: discoverable as "Spritz Media Server" on the local network
SSDP: listening on 239.255.255.250:1900
```

DLNA clients — smart TVs, Apple TV (via Infuse or VLC), PS5, Xbox, Kodi, and the like — should see it appear in their network sources within a few seconds.


# What is Spritz?

Spritz:
- Runs from a single command — no config files, no database, no nonsense
- Announces itself via SSDP/UPnP, so clients find it without you typing an IP
- Serves one or more folders (recursively — subdirectories are indexed automatically) from a single instance
- Presents three browse views at the root — **Videos**, **Music**, and **By folder** (the on-disk structure)
- Supports video *and* audio formats (MP4/MKV/AVI/MOV/… and MP3/FLAC/OGG/…)
- Implements `ContentDirectory:1` and `ConnectionManager:1` for DLNA Browse
- Also exposes a M3U playlist at `/spritz` for VLC, Infuse, and similar players
- Sends `ssdp:byebye` on Ctrl+C so clients drop it immediately

---

# Install

### From source

You'll need Rust — grab it from [Rustup](https://rustup.rs/). Open your terminal, navigate to e.g. ~/, then:

```bash
git clone https://github.com/dfallman/spritz
cd spritz
cargo install --path cli
```

# Usage

```
spritz [FOLDERS]... [OPTIONS]

Arguments:
  [FOLDERS]...  Folders to serve (defaults to the current directory)

Options:
  -p, --port <PORT>  Port to listen on [default: 8080]
  -h, --help         Print help
```

### Examples

```bash
# Serve the current directory
spritz

# Serve a specific folder
spritz /mnt/nas/movies

# Serve multiple folders
spritz /mnt/nas/movies /mnt/nas/tv /mnt/nas/kids

# Use a different port
spritz --port 9000 /media/videos
```

---

# Connecting a client

**Smart TVs, game consoles, or media players (DLNA).** Open your TV's Media Server or Network source. Spritz should show up within a few seconds. Inside you'll see three containers — `Videos`, `Music`, and `By folder`. The first two are flat lists of every file by type; `By folder` mirrors your on-disk directory structure so you can navigate Shows → Season 1 → ep1.mkv the way you'd expect.

**VLC.** `Media → Open Network Stream → http://192.168.x.x:8080/spritz`, or browse via `View → Playlist → Local Network → Universal Plug'n'Play`. VLC only scans at startup and when it receives a NOTIFY packet, so if it doesn't appear, restart VLC after Spritz is already running.

**Infuse (Apple TV / iOS / iPadOS).** `Add Files → Network Share` and pick Spritz Media Server from the list, or enter the M3U URL manually. Works on tvOS as well — the share browses the three-container layout described above.

**Any M3U-capable player.** Point it at `http://<your-ip>:8080/spritz`.

---

## Supported formats

**Video**

| Extension      | MIME type            |
|----------------|----------------------|
| `.mp4`, `.m4v` | `video/mp4`          |
| `.mkv`         | `video/x-matroska`   |
| `.avi`         | `video/x-msvideo`    |
| `.mov`         | `video/quicktime`    |
| `.webm`        | `video/webm`         |
| `.flv`         | `video/x-flv`        |

**Audio**

| Extension        | MIME type            |
|------------------|----------------------|
| `.mp3`           | `audio/mpeg`         |
| `.m4a`           | `audio/mp4`          |
| `.aac`           | `audio/aac`          |
| `.flac`          | `audio/flac`         |
| `.ogg`, `.oga`, `.opus` | `audio/ogg`   |
| `.wav`           | `audio/wav`          |
| `.wma`           | `audio/x-ms-wma`     |
| `.aiff`, `.aif`  | `audio/aiff`         |

---

### Windows/WSL2 notes
Spritz works under WSL2 with mirrored networking. Add this to your `~/.wslconfig`:

```ini
[wsl2]
networkingMode=mirrored
```

Then open the required ports in Windows Firewall (PowerShell, running as Administrator):

```powershell
New-NetFirewallRule -DisplayName "Spritz HTTP" `
  -Direction Inbound -Protocol TCP -LocalPort 8080 -Action Allow
New-NetFirewallRule -DisplayName "Spritz SSDP" `
  -Direction Inbound -Protocol UDP -LocalPort 1900 -Action Allow
```

> **Known issue:** SSDP multicast sometimes requires elevated privileges under WSL2. If devices don't auto-discover the server, try `sudo spritz` or grant the binary `CAP_NET_RAW`. HTTP file serving and the M3U endpoint work either way.

---

### Under the hood

Spritz implements DLNA/UPnP AV directly rather than wrapping an existing library:

**Discovery (SSDP).** On startup, Spritz sends `ssdp:alive` announcements to `239.255.255.250:1900`, listens for `M-SEARCH` requests, and responds unicast. Announcements repeat every 3 minutes so flaky WiFi clients get more chances to catch them. M-SEARCH responses honor the client's `MX` delay (UPnP 1.0 §1.2.3) and each NT is sent three times with small gaps to survive datagram loss on WiFi. On exit it sends `ssdp:byebye`.

**Device description.** `GET /upnp/description.xml` returns a UPnP `MediaServer:1` device description listing the ContentDirectory and ConnectionManager services. The `<dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>` element identifies the server as a DLNA DMS so strict clients (tvOS Infuse, SenPlayer) accept it.

**Browse (SOAP).** `POST /upnp/control/contentdirectory` handles `Browse`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities`. The root container has three children — `V` (Videos, flat list of video files), `A` (Music, flat list of audio files), and `F` (By folder, recursive directory mirror). Empty containers are hidden. `<res>` tags carry DLNA.ORG flags (`OP=01` byte-seek, plus the standard streaming FLAGS) and `size=`, and file responses set `transferMode.dlna.org: Streaming` + `contentFeatures.dlna.org` so Infuse accepts the stream.

**File serving.** Each source directory is mounted at `/m/{index}/`. Files stream over HTTP with range-request support, handled by `tower-http`'s `ServeDir`.

### Device notes

- **Samsung (Tizen):** expects `<dc:date>` on each DIDL item — included.
- **LG (webOS):** browsing works, but you may see an "unknown device" icon since there's no icon endpoint yet.
- **Sony / Bravia:** strict about `Content-Type: text/xml; charset="utf-8"` — handled.
- **Apple TV — Infuse.** Works on both tvOS and iOS/iPadOS. The strict client needed the full DIDL treatment (Videos/Music containers, size/flags on `<res>`, DLNA response headers) before playback worked on tvOS.
- **Apple TV — VLC.** Works on iOS/iPadOS via DLNA. tvOS VLC occasionally misses the SSDP announcement depending on the AP; if it doesn't appear, add the M3U URL manually.

# Troubleshooting
If clients (apps, TVs, etc.) can't find Spritz, check firewall rules first. SSDP needs UDP 1900, and HTTP needs whatever port you're serving on (8080 by default).

For a quick sanity check that bypasses discovery entirely, paste the M3U URL directly into VLC: `Media → Open Network Stream → http://192.168.X.X:8080/spritz`. If that plays, the server is fine and the issue is with discovery.

Restart VLC *after* Spritz is running, not before — VLC only scans at startup and when it receives a NOTIFY packet.


## License
MIT — see [LICENSE](LICENSE).
