# Spritz

[![ci](https://github.com/dfallman/spritz/actions/workflows/ci.yml/badge.svg)](https://github.com/dfallman/spritz/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/dfallman/spritz)](https://github.com/dfallman/spritz/releases)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`spritz` is a terminal-based, nano DLNA media server. Run it in a folder, and that folder's video and audio files become available to TVs, phones, speakers, and other media players on your local network using the DLNA protocol.

<img width="800" alt="spritz" src="https://github.com/user-attachments/assets/a021885f-d242-4118-a128-c115186879ec" />

### Quick start
Install:
```bash
brew install dfallman/tap/spritz
```

Navigate to the folder your media files are in and start Spritz:
```
cd /mnt/nas/movies
spritz
```

...alternatively, run it from anywhere and specify which path(s) to serve up:
```
spritz /mnt/nas/movies /mnt/nas/music
```

DLNA clients (including most modern TVs, Apple TV (via Infuse or VLC), PS5, Xbox, Kodi, tablets, phones, and many more) should now see the Spritz share appear in their network sources within a few seconds. 

## Features

- Runs from a single command, `spritz` — zero config, zero state
- Announces itself via SSDP/UPnP, so clients find it without typing an IP
- Serves one or more folders (recursively — subdirectories are indexed automatically) from a single instance
- Presents three browse views at the root: `Videos`, `Music`, and `By folder` (the on-disk structure)
- Supports video and audio formats (MP4/MKV/AVI/MOV/… and MP3/FLAC/OGG/…)
- Implements `ContentDirectory:1` and `ConnectionManager:1` for DLNA Browse and Search
- Advertises sidecar subtitles (`.srt` / `.vtt` / `.ass`) next to matching media files
- Advertises duration, resolution, honest DLNA profile names (from the file, not the extension), and sidecar album art (`cover.jpg` / matching stem)
- Exposes an M3U playlist at `/spritz` for VLC, Infuse, and similar players
- Sends `ssdp:byebye` on Ctrl+C so clients drop it immediately

## Why Spritz?

Most DLNA and media servers (such as Plex, Jellyfin, Emby, MiniDLNA/ReadyMedia, Rygel, Serviio, and others) are meant to be persistent servers: that is, you install a service, point a config file at your media library, often on a NAS or similar, maintain a database, and leave it running.

Spritz is the opposite — you've downloaded a file, you point spritz opportunistically at the folder that file is in, share it for as long as you need, and Ctrl+C when done. As we say in Australia, "no dramas": there are no config files, no database, no indexing and reindexing jobs, and no background services.

While Spritz is primarily meant as a means for serving up folders ad hoc, that is folders you don't serve every day, it _can_ also be used as a no-nonsense alternative to the heavier, more complex media file servers mentioned above. You can just leave it running on your NAS's `Media` folder too and spritz will do its thing.

**Note**: If you leave it running, note that Spritz parses the file tree at startup, but it doesn't monitor it for changes. Hence, if you add a file to a share, just restart Spritz. Alternatively, if you want to be fancy pants, try using something like [watchexec](https://github.com/watchexec/watchexec): `watchexec -r -- spritz` 

## Install

### Homebrew (macOS and Linux)

```bash
brew install dfallman/tap/spritz
```

### Pre-built binaries

Download the latest archive for your platform from the [Releases](https://github.com/dfallman/spritz/releases) page. Builds are provided for:

- Linux x86_64 and aarch64
- macOS x86_64 and Apple Silicon
- Windows x86_64

Each archive ships with a `.sha256` checksum.

### From source

To compile Spritz from source, you'll need Rust. Use [rustup](https://rustup.rs/) for the installation:
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

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
      --bind <ADDR>  Address to bind [default: 0.0.0.0]
  -n, --name <NAME>  Friendly name shown to DLNA clients [default: Spritz Media Server]
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

# Bind a specific interface and set the name TVs display
spritz --bind 192.168.1.10 --name "Living Room" /media/videos
```

## Connecting a client

### Smart TVs, game consoles, or media players (DLNA)
Open your TV's Media Server or Network source. Spritz should show up within a few seconds. Inside, you'll see three containers — `Videos`, `Music`, and `By folder`. The first two are flat lists of every file by type; `By folder` mirrors your on-disk directory structure so you can navigate Shows → Season 1 → ep1.mkv the way you'd expect.

### VLC
`Media → Open Network Stream → http://<your-spritz-server-ip>:8080/spritz`, or browse via `View → Playlist → Local Network → Universal Plug and Play`. VLC only scans at startup and when it receives a NOTIFY packet, so if it doesn't appear, restart VLC once Spritz is already running.

### Infuse (Apple TV / iOS / iPadOS)
`Add Files → Network Share` and pick Spritz Media Server from the list, or enter the M3U URL manually. Works on tvOS as well — the share browses the three-container layout described above.

### Any M3U-capable player
Point it at `http://<your-spritz-server-ip>:8080/spritz`.

**Note**: `<your-spritz-server-ip>` is the IP address of the machine you run spritz on, that's the "server" here — the "client" (see above) is the device from which you play your content. To find your server's IP, use `ipconfig` in the Windows Command Prompt. On macOS, check `System Settings → Network` (or use `ipconfig getifaddr en0` in a terminal). On linux, run `ip addr` or `hostname -I`.


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

**Subtitles** (sidecar files next to a video/audio item; not listed as their own library entries)

| Extension      | MIME type     |
|----------------|---------------|
| `.srt`         | `text/srt`    |
| `.vtt`         | `text/vtt`    |
| `.ass`, `.ssa` | `text/x-ssa`  |

## Compatibility

| Device                                | Status | Notes                                                              |
|---------------------------------------|--------|--------------------------------------------------------------------|
| Samsung (Tizen)                       | Works  | Requires `<dc:date>` on each DIDL item — included                  |
| LG (webOS)                            | Works  | Device icon served at `/upnp/icon.png`                             |
| Sony / Bravia                         | Works  | Strict about `Content-Type: text/xml; charset="utf-8"` — handled   |
| Apple TV — Infuse (tvOS, iOS, iPadOS) | Works  | Requires the full DIDL treatment for tvOS playback                 |
| Apple TV — VLC (tvOS, iOS, iPadOS)    | Works  | tvOS VLC sometimes misses SSDP; add the M3U URL manually           |
| Xbox                                  | Works  | Advertises Microsoft `MediaReceiverRegistrar`                      |

## Troubleshooting

DLNA is fiddly by nature, especially in combination with certain devices and operating systems (looking at you, Apple TV).

If your client can't find Spritz, check your firewall rules first (on both the server and client side, but typically the server side): SSDP needs UDP 1900 open, and HTTP needs your serving port (8080 by default).

On a VPN, Docker, or multi-homed machine, discovery may succeed while playback URLs point at the wrong address. Infuse and VLC using the M3U URL (`/spritz`) pick up the `Host` header; SSDP `LOCATION` tries to reply with the interface on the same subnet as the client.

IPv6-only LANs: SSDP also joins `[FF02::C]:1900`. When `--bind` is the default unspecified address, HTTP tries a dual-stack socket so IPv6 clients can fetch `LOCATION`. Link-local (`fe80::`) addresses are not advertised.

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

- **Discovery (SSDP).** Sends `ssdp:alive` on IPv4 and IPv6 on startup, responds to `M-SEARCH`, and sends `ssdp:byebye` on exit.
- **Device description.** `GET /upnp/description.xml` returns a `MediaServer:1` description advertising ContentDirectory, ConnectionManager, and Xbox MediaReceiverRegistrar.
- **Browse / Search (SOAP).** `POST /upnp/control/contentdirectory` handles `Browse`, `Search`, and related actions, exposing three root containers: Videos (flat), Music (flat), and By folder (recursive).
- **File serving.** Each source directory is mounted at `/m/{index}/` and served over HTTP with range support. Only media, sidecar-subtitle, and album-art extensions are reachable; directory listings, other files, and symlinks are not.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full protocol walkthrough.

## Note on AI use

The author of this application has been writing code for over 30 years. Lately, LLM agent-enhanced coding practices have rekindled my sense of awe at what's possible. This project has been built using a range of tools, with Anthropic's Claude Code (using Opus 4.7) among them.

Unlike some who dismiss anything touched by a coding agent as "slop," I don't see it that way. To me, these tools are a way to move much faster, explore many more ideas, and test those ideas and implementations more rigorously than I ever could on my own.

## Contributing

Issues and pull requests welcome. For bugs, please include your client device/OS, firewall setup, and the output of `spritz` when the client tries to connect.

## License

MIT — see [LICENSE](LICENSE).
