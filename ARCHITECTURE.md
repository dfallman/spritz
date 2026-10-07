# Architecture

Spritz implements DLNA/UPnP AV directly instead of wrapping an existing library. This document walks through each protocol layer.

## Discovery (SSDP)

Spritz sends `ssdp:alive` announcements to `239.255.255.250:1900` (IPv4) and `[FF02::C]:1900` (IPv6) on startup, responds to `M-SEARCH` requests (honoring the client's `MX` delay per UPnP 1.0 §1.2.3), and sends `ssdp:byebye` on exit. Announcements repeat every 3 minutes, and each NT is sent three times with small gaps to survive datagram loss on WiFi. `LOCATION` URLs use `[ipv6]:port` when answering an IPv6 search.

## Device description

`GET /upnp/description.xml` returns a `MediaServer:1` description advertising ContentDirectory, ConnectionManager, and Microsoft `X_MS_MediaReceiverRegistrar` (Xbox). The `<dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>` tag marks it as a DLNA DMS, which strict clients (tvOS Infuse, SenPlayer) require.

## Browse (SOAP)

`POST /upnp/control/contentdirectory` handles `Browse`, `Search`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities`. The root has three children: `V` (Videos, flat), `A` (Music, flat), and `F` (By folder, recursive). Empty containers are hidden. `<res>` tags include `size=`, `duration=` when the container header can be parsed, `resolution=` when width/height are known, a `DLNA.ORG_PN` only when the probed codec is a known DLNA profile (H.264 bands; HEVC/VP9/AV1 omit the PN rather than lie), and DLNA.ORG flags (`OP=01` byte-seek plus standard streaming flags). Matching sidecar subtitles (`<stem>.<ext>` and tagged `<stem>.<tag>[.<tag>].<ext>`, found with one listing per folder by `spritz_core::sidecar_subtitles`) are extra `<res>` URLs, untagged first. Sidecar covers (`cover.jpg` / same-stem `.jpg`) appear as `<upnp:albumArtURI>` pointing at `/art/{index}`. File responses set `transferMode.dlna.org: Streaming` and `contentFeatures.dlna.org` so Infuse will play them.

A `SUBSCRIBE` to an event URL is answered with a SID and an immediate HTTP `NOTIFY` carrying the current state variables. `SystemUpdateID` lives in `EventHub` and starts at `1`. The CLI scans once at start, so for it the value never changes. An embedder that swaps the library (Spritz Server) calls `EventHub::content_changed`, which bumps the id, reports it in `GetSystemUpdateID` and in Browse/Search `UpdateID`, and sends a `NOTIFY` (SEQ 1, 2, …) to every live ContentDirectory subscriber.

`GET /upnp/icon.png` is a 48×48 PNG listed in `iconList` on the device description.

## File serving

Each source directory is mounted at `/m/{index}/` and served over HTTP with range support via `tower-http`'s `ServeFile`. Requests that leave the tree, follow a symlink, or use an unknown extension return 404. Sidecar subtitles are reachable like media (any `.srt`, `.vtt`, `.ass`, or `.ssa` inside a source directory) so clients can fetch the extra `<res>` URLs. Album art is served at `/art/{index}` from `cover.jpg` / `folder.jpg` / a same-stem image next to the file.

## Spritz protocol

Spritz players use a small protocol of their own next to DLNA, for networks where SSDP multicast does not get through. [PROTOCOL.md](PROTOCOL.md) is the full description; this is where each part lives.

- **Identity endpoint** (`api::identity`). `GET /.well-known/spritz` returns JSON naming the server: protocol version, product and version, friendly name, UPnP UDN (with the `uuid:` prefix), HTTP port, description path, `.local` hostname, and the LAN IPv4 addresses. The addresses are read per request, the advertised one first, and exclude loopback, link-local, tunnel and virtual interfaces; with `--bind` set to one address they are just that address.
- **Bonjour** (`api::bonjour`). The server registers `_spritz._tcp` with TXT `proto=1`, `uuid=<UDN>` and `desc=/upnp/description.xml`. On macOS this goes through mDNSResponder (`DNSServiceRegister` from libSystem, no crate), so the Bonjour Sleep Proxy can keep a sleeping Mac reachable; elsewhere it uses `mdns-sd`. The guard withdraws the service on drop and reports the registered name after any rename.
- **Client tracking** (`dlna::clients`, `api::track`). The SSDP handler records each M-SEARCH it would answer, and an HTTP middleware maps every request to a stage: found (identity, description, service XML, M3U), browsed (SOAP control and eventing), streamed (media and art). Peers are keyed by IP, with IPv4-mapped IPv6 folded into IPv4. Searches from unknown non-Spritz agents are ignored, a non-Spritz agent never renames a Spritz client, and every client-supplied string is stripped of control and invisible characters and capped. Records do not expire; at most 64 are kept. A Spritz client whose first unanswered search is 15 s old, and whose latest search is under 10 minutes old, is diagnosed as blocked by the firewall.
- **Local peers only** (`api::peers`). An outermost middleware answers `403` to peers that are neither local by range (loopback, private IPv4, `100.64.0.0/10`, link-local, IPv6 ULA; IPv4-mapped folded into IPv4) nor on a directly attached subnet; only public peers cost an interface lookup. Without a peer address it refuses, so a wiring mistake fails closed. `--allow-remote` leaves the layer out. SSDP is not filtered: it only answers on the multicast groups and to M-SEARCH senders, and the `LOCATION` it hands out is itself behind the filter.
- **Network checks** (`api::netcheck`). Every 5 s the server lists the interfaces that are up and reports what it can see: no LAN address, Local Network access denied (macOS, from a probe to the subnet gateway), UDP 1900 taken, Bonjour failing, a VPN carrying the advertised address (a note when the VPN is only alongside it), more than one LAN subnet, and (with `--allow-remote` only) any public address this computer has. The CLI prints client progress and check changes through `api::report`; the macOS app reads the same data through its FFI.
