# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
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

- Add audio support and restructure DIDL hierarchy for Infuse tvOS

## [0.1.1] - 2026-04-22

### Added

- Add audio support and restructure DIDL hierarchy for Infuse tvOS

<!-- release-plz inserts new version sections below this line -->
