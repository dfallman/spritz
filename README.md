# Spritz

[![ci](https://github.com/dfallman/spritz/actions/workflows/ci.yml/badge.svg)](https://github.com/dfallman/spritz/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/dfallman/spritz)](https://github.com/dfallman/spritz/releases)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`spritz` is a terminal-based, nano DLNA media server. Run it in a folder and that folder's video and audio becomes instantly available from TVs, phones, speakers, and other media players on your local network.

<img width="1684" height="1148" alt="spritz-github" src="https://github.com/user-attachments/assets/32228ce0-14c6-4a5a-9222-95802132ec17" />

### Usage
```
cd /mnt/nas/movies
spritz
```

or, alternatively, run it from anywhere with one or many paths to share:

```
spritz /mnt/nas/movies /mnt/nas/music
```

DLNA clients — such as Smart TVs, Apple TV (via Infuse or VLC), PS5, Xbox, Kodi, and the like — should see it appear in their network sources within a few seconds.


## Features

- Runs from a single command, `spritz` — zero config, zero state
- Announces itself via SSDP/UPnP, so clients find it without typing an IP
- Serves one or more folders (recursively — subdirectories are indexed automatically) from a single instance
- Presents three browse views at the root: `Videos`, `Music`, and `By folder` (the on-disk structure)
- Supports video and audio formats (MP4/MKV/AVI/MOV/… and MP3/FLAC/OGG/…)
- Implements `ContentDirectory:1` and `ConnectionManager:1` for DLNA Browse
- Exposes an M3U playlist at `/spritz` for VLC, Infuse, and similar players
- Sends `ssdp:byebye` on Ctrl+C so clients drop it immediately

## Why Spritz?

Most DLNA servers (MiniDLNA/ReadyMedia, Rygel, Serviio, Plex) are persistent daemons: install a service, point a config file at your media library, maintain a database, and leave it running. Spritz is the opposite — point it at a folder, share it for as long as you need, Ctrl+C when done. No config, no database, no indexing job, no background service. 

While Spritz is primarily meant as a means for serving up laptop folders, ad-hoc shares, and other folders you don't serve every day, it can also be used as a no-nonsense alternative to heavier, more complex media file servers mentioned above.

**Note**: Spritz parses the file tree when it starts up, but it doesn't monitor it for changes. Hence, if you add a file to a share, it won't show until you restart Spritz.


## Install

### Pre-built binaries

Download the latest archive for your platform from the [Releases](https://github.com/dfallman/spritz/releases) page. Builds are provided for:

- Linux x86_64 and aarch64
- macOS x86_64 and Apple Silicon
- Windows x86_64

Each archive ships with a `.sha256` checksum.

### From source

To compile Spritz, you’ll need the Rust toolchain. Use [rustup](https://rustup.rs/) for the installation; it ensures you have the latest version of cargo, whereas system-level managers (like apt) frequently distribute stale binaries that may lead to compatibility issues.

Once installed:
```bash
git clone https://github.com/dfallman/spritz
cd spritz
cargo install --path cli
```

## Usage

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

## Connecting a client

- **Smart TVs, game consoles, or media players (DLNA).** Open your TV's Media Server or Network source. Spritz should show up within a few seconds. Inside you'll see three containers — `Videos`, `Music`, and `By folder`. The first two are flat lists of every file by type; `By folder` mirrors your on-disk directory structure so you can navigate Shows → Season 1 → ep1.mkv the way you'd expect.

- **VLC.** `Media → Open Network Stream → http:/<your-spritz-server-ip>:8080/spritz`, or browse via `View → Playlist → Local Network → Universal Plug'n'Play`. VLC only scans at startup and when it receives a NOTIFY packet, so if it doesn't appear, restart VLC once Spritz is already running.

- **Infuse (Apple TV / iOS / iPadOS).** `Add Files → Network Share` and pick Spritz Media Server from the list, or enter the M3U URL manually. Works on tvOS as well — the share browses the three-container layout described above.

- **Any M3U-capable player.** Point it at `http://<your-spritz-server-ip>:8080/spritz`.

**Note**: to find your server's IP, use `ipconfig` in the Windows Command Prompt, check `System Settings → Network` (or use `ipconfig getifaddr en0` in a terminal) on macOS, and run `ip addr` or `hostname -I` on Linux.


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

| Extension               | MIME type       |
|-------------------------|-----------------|
| `.mp3`                  | `audio/mpeg`    |
| `.m4a`                  | `audio/mp4`     |
| `.aac`                  | `audio/aac`     |
| `.flac`                 | `audio/flac`    |
| `.ogg`, `.oga`, `.opus` | `audio/ogg`     |
| `.wav`                  | `audio/wav`     |
| `.wma`                  | `audio/x-ms-wma`|
| `.aiff`, `.aif`         | `audio/aiff`    |

## Compatibility

| Device                                | Status | Notes                                                              |
|---------------------------------------|--------|--------------------------------------------------------------------|
| Samsung (Tizen)                       | Works  | Requires `<dc:date>` on each DIDL item — included                  |
| LG (webOS)                            | Works  | Shows an "unknown device" icon (no icon endpoint yet)              |
| Sony / Bravia                         | Works  | Strict about `Content-Type: text/xml; charset="utf-8"` — handled   |
| Apple TV — Infuse (tvOS, iOS, iPadOS) | Works  | Required the full DIDL treatment for tvOS playback                 |
| Apple TV — VLC (iOS, iPadOS)          | Works  | tvOS VLC sometimes misses SSDP; add the M3U URL manually           |

## Troubleshooting

DLNA is fiddly by nature, especially combined with some devices and operating systems (looking at you, Apple TV).

If your client can't find Spritz, check your firewall rules first (on both the server and client side, but typically the server side): SSDP needs UDP 1900 open, and HTTP needs your serving port (8080 by default).

On Apple TV, Infuse tends to work better than VLC. If you're using VLC and can't find the share, you can bypass discovery entirely by pasting the M3U URL into VLC: `Media → Open Network Stream → http://192.168.X.X:8080/spritz`. If that plays, the server is fine and the issue is discovery.

Restart VLC once Spritz is already running — VLC only scans at startup and on NOTIFY packets.

### Windows / WSL2

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

## Architecture

Spritz implements DLNA/UPnP AV directly instead of wrapping an existing library. At a glance:

- **Discovery (SSDP).** Sends `ssdp:alive` on startup, responds to `M-SEARCH`, and sends `ssdp:byebye` on exit.
- **Device description.** `GET /upnp/description.xml` returns a `MediaServer:1` description advertising ContentDirectory and ConnectionManager.
- **Browse (SOAP).** `POST /upnp/control/contentdirectory` handles `Browse` and related actions, exposing three root containers: Videos (flat), Music (flat), and By folder (recursive).
- **File serving.** Each source directory is mounted at `/m/{index}/` and served over HTTP with range support.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full protocol walkthrough.

## Contributing

Issues and pull requests welcome. For bugs, please include your client device/OS, firewall setup, and the output of `spritz` when the client tries to connect.

## License

MIT — see [LICENSE](LICENSE).
