# Architecture

Spritz implements DLNA/UPnP AV directly instead of wrapping an existing library. This document walks through each protocol layer.

## Discovery (SSDP)

Spritz sends `ssdp:alive` announcements to `239.255.255.250:1900` (IPv4) and `[FF02::C]:1900` (IPv6) on startup, responds to `M-SEARCH` requests (honoring the client's `MX` delay per UPnP 1.0 §1.2.3), and sends `ssdp:byebye` on exit. Announcements repeat every 3 minutes, and each NT is sent three times with small gaps to survive datagram loss on WiFi. `LOCATION` URLs use `[ipv6]:port` when answering an IPv6 search.

## Device description

`GET /upnp/description.xml` returns a `MediaServer:1` description advertising ContentDirectory, ConnectionManager, and Microsoft `X_MS_MediaReceiverRegistrar` (Xbox). The `<dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>` tag marks it as a DLNA DMS, which strict clients (tvOS Infuse, SenPlayer) require.

## Browse (SOAP)

`POST /upnp/control/contentdirectory` handles `Browse`, `Search`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities`. The root has three children: `V` (Videos, flat), `A` (Music, flat), and `F` (By folder, recursive). Empty containers are hidden. `<res>` tags include `size=`, `duration=` when the container header can be parsed, `resolution=` when width/height are known, a `DLNA.ORG_PN` only when the probed codec is a known DLNA profile (H.264 bands; HEVC/VP9/AV1 omit the PN rather than lie), and DLNA.ORG flags (`OP=01` byte-seek plus standard streaming flags). Matching sidecar subtitles (`.srt` / `.vtt` / `.ass`) are extra `<res>` URLs. Sidecar covers (`cover.jpg` / same-stem `.jpg`) appear as `<upnp:albumArtURI>` pointing at `/art/{index}`. File responses set `transferMode.dlna.org: Streaming` and `contentFeatures.dlna.org` so Infuse will play them.

A `SUBSCRIBE` to an event URL is answered with a SID and an immediate HTTP `NOTIFY` carrying the current state variables (`SystemUpdateID` stays `1` because the library is scanned once at start).

`GET /upnp/icon.png` is a 48×48 PNG listed in `iconList` on the device description.

## File serving

Each source directory is mounted at `/m/{index}/` and served over HTTP with range support via `tower-http`'s `ServeFile`. Requests that leave the tree, follow a symlink, or use an unknown extension return 404. Sidecar subtitles sharing a stem with an indexed file are reachable so clients can fetch the extra `<res>` URLs. Album art is served at `/art/{index}` from `cover.jpg` / `folder.jpg` / a same-stem image next to the file.
