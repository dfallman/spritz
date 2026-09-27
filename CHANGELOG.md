# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
## [Unreleased]

## [0.1.9] - 2026-09-27

### Added

- Spritz protocol identity endpoint at `/.well-known/spritz`: name, UPnP UDN, port, LAN addresses and `.local` hostname
- Advertise the server over Bonjour as `_spritz._tcp` (through mDNSResponder on macOS, so the Bonjour Sleep Proxy can wake a sleeping Mac)
- Track Spritz clients from SSDP searches and HTTP requests, and print each client's progress: found, browsing, streaming
- Diagnose players that search but never connect, and players that advertise `_spritz-player._tcp` but never connect: "Check the firewall on this Mac"
- Network self-checks at startup and every 5 s: no LAN address, Local Network access denied (macOS), UDP 1900 unavailable, Bonjour failure, VPN, several LAN subnets
- Expose the server's bound addresses to embedders

### Fixed

- Strip control, bidi and other invisible characters from client names and user agents before printing them
- Honour a specific `--bind` in the identity addresses and the DLNA line

### Documentation

- Describe the spritz protocol in PROTOCOL.md

## [0.1.8] - 2026-09-22

### Added

- Show a live "Scanning media files... (N)" counter during startup
- Add audio support and restructure DIDL hierarchy for Infuse tvOS

### Fixed

- Harden media serving, probing, SSDP and eventing against untrusted input
- Probe MP4/MKV headers without reading the whole file
- Fix to probing video files

## [0.1.7] - 2026-09-21

### Added

- Add audio support and restructure DIDL hierarchy for Infuse tvOS

### Fixed

- Probe MP4/MKV headers without reading the whole file
- Fix to probing video files

## [0.1.6] - 2026-09-21

### Added

- Add audio support and restructure DIDL hierarchy for Infuse tvOS

### Fixed

- Fix to probing video files

## [0.1.5] - 2026-09-01

### Added

- Add audio support and restructure DIDL hierarchy for Infuse tvOS

### Fixed

- Fix to probing video files

### Added

- `--bind` and `--name` so a multi-homed host can pick an interface and a DLNA friendly name
- ContentDirectory `Search` (title contains + video/audio class)
- 48×48 PNG device icon at `GET /upnp/icon.png`
- Sidecar subtitles (`.srt` / `.vtt` / `.ass`) served over HTTP and advertised as extra DIDL `<res>` tags
- Access log (`info`) for `/m/`, `/upnp/`, and `/art/` requests
- DIDL `duration` and codec-accurate `DLNA.ORG_PN` from file headers (MP4/MKV/WAV/FLAC), plus `resolution=` when width/height are known. HEVC/VP9/AV1 omit the PN rather than claiming H.264. Sidecar album art.
- UPnP event `NOTIFY` after `SUBSCRIBE` (ContentDirectory, ConnectionManager, MediaReceiverRegistrar)
- Xbox `X_MS_MediaReceiverRegistrar` service (`IsAuthorized` / `IsValidated` always succeed)
- IPv6 SSDP (`[FF02::C]:1900`) and dual-stack HTTP when `--bind` is unspecified

## [0.1.4] - 2026-04-22

### Added

- `GET /health` endpoint for heartbeat checks
- Structured logging via `tracing` (`RUST_LOG` env-filter supported)

### Changed

- Cap SOAP request bodies at 64 KiB and apply a 30 s timeout on `/upnp/*` routes
- Release binaries now embed their dependency tree via `cargo-auditable`
- Remove the last `unsafe` block in the codebase (SSDP socket conversion)

## [0.1.3] - 2026-04-22

### Added

- Add audio support and restructure DIDL hierarchy for Infuse tvOS

## [0.1.2] - 2026-04-22

### Added

- Cross-platform release binaries via cargo-dist

## [0.1.1] - 2026-04-22

### Added

- Initial public release automation (release-plz)

<!-- release-plz inserts new version sections below this line -->
