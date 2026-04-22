# Architecture

Spritz implements DLNA/UPnP AV directly instead of wrapping an existing library. This document walks through each protocol layer.

## Discovery (SSDP)

Spritz sends `ssdp:alive` announcements to `239.255.255.250:1900` on startup, responds to `M-SEARCH` requests (honoring the client's `MX` delay per UPnP 1.0 §1.2.3), and sends `ssdp:byebye` on exit. Announcements repeat every 3 minutes, and each NT is sent three times with small gaps to survive datagram loss on WiFi.

## Device description

`GET /upnp/description.xml` returns a `MediaServer:1` description advertising the ContentDirectory and ConnectionManager services. The `<dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>` tag marks it as a DLNA DMS, which strict clients (tvOS Infuse, SenPlayer) require.

## Browse (SOAP)

`POST /upnp/control/contentdirectory` handles `Browse`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities`. The root has three children: `V` (Videos, flat), `A` (Music, flat), and `F` (By folder, recursive). Empty containers are hidden. `<res>` tags include `size=` and DLNA.ORG flags (`OP=01` byte-seek plus standard streaming flags); file responses set `transferMode.dlna.org: Streaming` and `contentFeatures.dlna.org` so Infuse will play them.

## File serving

Each source directory is mounted at `/m/{index}/` and served over HTTP with range support via `tower-http`'s `ServeDir`.
