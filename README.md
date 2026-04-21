# Spritz

`spritz` is a nano DLNA media server. Run it in a folder and that folder becomes browsable from TVs, phones, and other media players on your local network.

```
cd /mnt/nas/movies
spritz
```
...or
```
spritz /mtt/nas/movies 
```

Expected output:
```
Indexed 47 video(s)
Serving on http://192.168.1.100:8080/spritz
DLNA: discoverable as "Spritz Media Server" on the local network
SSDP: listening on 239.255.255.250:1900
```

DLNA clients — smart TVs, Apple TV (via Infuse or VLC), PS5, Xbox, Kodi, and the like — should see it appear in their network sources within a few seconds.


# What is Spritz?

Spritz:
- Runs from a single command — no config files, no database, no nonsense
- Announces itself via SSDP/UPnP, so clients find it without you typing an IP
- Serves one or more folders from a single instance
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

**Smart TVs, game consoles, or media players (DLNA).** Open your TV's Media Server or Network source. Spritz should show up within a few seconds.

**VLC.** `Media → Open Network Stream → http://192.168.x.x:8080/spritz`, or browse via `View → Playlist → Local Network → Universal Plug'n'Play`. VLC only scans at startup and when it receives a NOTIFY packet, so if it doesn't appear, restart VLC after Spritz is already running.

**Infuse (Apple TV / iOS).** `Add Files → Network Share` and pick Spritz Media Server from the list, or enter the M3U URL manually.

**Any M3U-capable player.** Point it at `http://<your-ip>:8080/spritz`.

---

## Supported formats

| Extension      | MIME type            |
|----------------|----------------------|
| `.mp4`, `.m4v` | `video/mp4`          |
| `.mkv`         | `video/x-matroska`   |
| `.avi`         | `video/x-msvideo`    |
| `.mov`         | `video/quicktime`    |
| `.webm`        | `video/webm`         |
| `.flv`         | `video/x-flv`        |

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
**Discovery (SSDP).** On startup, Spritz sends `ssdp:alive` announcements to `239.255.255.250:1900`, listens for `M-SEARCH` requests, and responds unicast. Announcements repeat every 15 minutes. On exit it sends `ssdp:byebye`.
**Device description.** `GET /upnp/description.xml` returns a UPnP `MediaServer:1` device description listing the ContentDirectory and ConnectionManager services.
**Browse (SOAP).** `POST /upnp/control/contentdirectory` handles `Browse`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities`. Browse returns DIDL-Lite XML with one `<item>` per video, XML-escaped inside a `<Result>` element as required by the ContentDirectory spec.
**File serving.** Each source directory is mounted at `/v/{index}/`. Files stream over HTTP with range-request support, handled by `tower-http`'s `ServeDir`.

### Device notes

- **Samsung (Tizen):** expects `<dc:date>` on each DIDL item — included.
- **LG (webOS):** browsing works, but you may see an "unknown device" icon since there's no icon endpoint yet.
- **Sony / Bravia:** strict about `Content-Type: text/xml; charset="utf-8"` — handled.
- **Apple TV (Infuse / VLC):** works via both DLNA and the M3U endpoint.

# Troubleshooting
If clients (apps, TVs, etc.) can't find Spritz, check firewall rules first. SSDP needs UDP 1900, and HTTP needs whatever port you're serving on (8080 by default).

For a quick sanity check that bypasses discovery entirely, paste the M3U URL directly into VLC: `Media → Open Network Stream → http://192.168.X.X:8080/spritz`. If that plays, the server is fine and the issue is with discovery.

Restart VLC *after* Spritz is running, not before — VLC only scans at startup and when it receives a NOTIFY packet.


## License
MIT
