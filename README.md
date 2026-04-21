# Spritz

Instant DLNA media server. Run it in a folder — that folder is immediately available on every TV, phone, and media player on your network. No config, no accounts, no cloud.

```
cd /mnt/nas/movies
spritz
```

```
Indexed 47 video(s)
Serving on http://192.168.1.100:8080/spritz
DLNA: discoverable as "Spritz Media Server" on the local network
SSDP: listening on 239.255.255.250:1900
```

Your Smart TV, Apple TV (via Infuse or VLC), PS5, Xbox, Kodi, and any other DLNA client will find **Spritz Media Server** in their network sources automatically.

---

## Features

- **Zero config** — one command, no setup files, no database
- **Auto-discovery** — SSDP/UPnP multicast; devices find it without an IP
- **Multi-folder** — serve multiple directories from a single instance
- **DLNA compliant** — ContentDirectory:1, ConnectionManager:1, full Browse support
- **M3U playlist** — `/spritz` endpoint for VLC, Infuse, and direct URL clients
- **Graceful shutdown** — Ctrl+C sends `ssdp:byebye` before exiting so devices clear it immediately

---

## Install

### From source

Get the latest Rust from [Rustup](https://rustup.rs/).

```bash
git clone https://github.com/dfallman/spritz
cd spritz
cargo install --path cli
```

Or just build and run locally:

```bash
cargo build --release
./target/release/cli
```

---

## Usage

```
spritz [FOLDERS]... [OPTIONS]

Arguments:
  [FOLDERS]...  Folders to serve (defaults to current directory)

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

# Serve multiple folders at once
spritz /mnt/nas/movies /mnt/nas/tv /mnt/nas/kids

# Use a different port
spritz --port 9000 /media/videos
```

---

## Connecting a client

### Smart TV / game console / media player (DLNA)

Just browse your TV's "Media Server" or "Network" source. **Spritz Media Server** will appear automatically within a few seconds.

### VLC

`Media → Open Network Stream` → `http://192.168.x.x:8080/spritz`

Or browse via `View → Playlist → Local Network → Universal Plug'n'Play`.

### Infuse (Apple TV / iOS)

`Add Files → Network Share` → select **Spritz Media Server** from the auto-discovered list, or add manually via the M3U URL above.

### Any M3U-capable player

Point it at `http://<your-ip>:8080/spritz`.

---

## Supported formats

| Extension | MIME type |
|---|---|
| `.mp4`, `.m4v` | `video/mp4` |
| `.mkv` | `video/x-matroska` |
| `.avi` | `video/x-msvideo` |
| `.mov` | `video/quicktime` |
| `.webm` | `video/webm` |
| `.flv` | `video/x-flv` |

---

## WSL2 notes

Spritz works under WSL2 with mirrored networking. Add these to `~/.wslconfig`:

```ini
[wsl2]
networkingMode=mirrored
```

Then open the required ports in Windows Firewall (run PowerShell as Administrator):

```powershell
New-NetFirewallRule -DisplayName "Spritz HTTP" `
  -Direction Inbound -Protocol TCP -LocalPort 8080 -Action Allow

New-NetFirewallRule -DisplayName "Spritz SSDP" `
  -Direction Inbound -Protocol UDP -LocalPort 1900 -Action Allow
```

> **Known issue:** SSDP multicast may require the server to run with elevated privileges under WSL2. If devices don't auto-discover the server, try `sudo spritz` or grant the binary `CAP_NET_RAW`. HTTP file serving and the M3U endpoint work regardless.

---

## How it works

Spritz implements a minimal DLNA/UPnP AV server from scratch — no third-party DLNA library.

```
┌─────────┐     ┌──────────────────────────────────────────┐
│  TV /   │     │                  Spritz                  │
│ player  │     │                                          │
│         │────▶│  SSDP (UDP 1900)   ← discovery          │
│         │     │  GET /upnp/description.xml               │
│         │────▶│  POST /upnp/control/contentdirectory     │
│         │     │    Browse → DIDL-Lite XML                │
│         │────▶│  GET /v/{dir}/{file}   ← stream          │
└─────────┘     └──────────────────────────────────────────┘
```

**Discovery (SSDP):** On startup, Spritz sends `ssdp:alive` multicast announcements to `239.255.255.250:1900`. It listens for `M-SEARCH` requests and responds unicast. Announcements repeat every 15 minutes. On exit, `ssdp:byebye` is sent.

**Device description:** `GET /upnp/description.xml` returns a UPnP MediaServer:1 device description listing the ContentDirectory and ConnectionManager services.

**Browse (SOAP):** `POST /upnp/control/contentdirectory` handles `Browse`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities` actions. Browse returns DIDL-Lite XML with one `<item>` per video file, embedded (XML-escaped) inside a `<Result>` element per the UPnP ContentDirectory spec.

**File serving:** Each source directory is mounted at `/v/{index}/`. Files are streamed directly via HTTP with full range-request support (courtesy of `tower-http`'s `ServeDir`).

---

## Project structure

```
spritz/
├── core/       Domain logic — video discovery, path encoding
├── api/        Axum HTTP server — M3U, file serving, DLNA integration
├── cli/        Clap CLI entry point
└── dlna/       DLNA/UPnP AV implementation
    ├── description.rs   Device + service XML templates
    ├── soap.rs          SOAP envelope parser/builder
    ├── content_dir.rs   ContentDirectory Browse handler
    └── ssdp.rs          SSDP multicast announcer + M-SEARCH responder
```

### Dependency graph

```
cli → api → dlna → core
         ↘ core
```

---

## Device compatibility notes

- **Samsung (Tizen):** Requires `<dc:date>` on each DIDL item — included.
- **LG (webOS):** May show "unknown device" icon (no icon endpoint implemented) but browsing works.
- **Sony/Bravia:** Strict about `Content-Type: text/xml; charset="utf-8"` — handled.
- **Apple TV (Infuse/VLC):** Works via both DLNA and the M3U endpoint.

---

## License

MIT
