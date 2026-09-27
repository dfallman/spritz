# The spritz protocol

Spritz speaks DLNA to everything. The spritz protocol is a small layer that Spritz servers and
Spritz players add on top, so they can find each other and explain failures when DLNA discovery
does not get through. This is version 1. When ContentDirectory's SOAP browse becomes the limit
(rich metadata, resume positions, per-device state), protocol 2 adds a JSON browse endpoint
under the same `/.well-known` contract rather than replacing the DLNA transport.

## Parity rule

Every server capability ships in both the `spritz` CLI and Spritz Server, implemented once in
the upstream crates; the GUI only presents it. The one exception: iCloud rendezvous needs an
app bundle and an iCloud entitlement, so only Spritz Server publishes it.

## Version 1

### Identity endpoint

`GET /.well-known/spritz` returns `200` with `Content-Type: application/json`:

```json
{
  "protocol": 1,
  "server": "spritz",
  "version": "0.1.9",
  "name": "Daniel's Mac mini",
  "uuid": "uuid:6f1c…",
  "port": 8080,
  "addresses": ["192.168.1.23", "10.0.0.5"],
  "description": "/upnp/description.xml",
  "hostname": "Daniels-Mac-mini.local"
}
```

- `server`: the host application, `"spritz"` (CLI) or `"spritz-server"` (macOS app).
- `version`: the host application's version.
- `uuid`: exactly the text of the UPnP description's `<UDN>` element, **including the `uuid:`
  prefix**. The Player keys servers on that text (`MediaServer.udn`), so identity, TXT, iCloud
  and SSDP results merge byte for byte.
- `addresses`: every IPv4 address of an up, non-loopback, non-link-local, non-tunnel,
  non-virtual interface (see Network self-checks, below), the advertised address first. A
  Player that reached the server one way stores the others. IPv6 would be added here later; the
  Player's Bonjour resolution is IPv4-only in v1 (see How a Spritz player finds a server,
  below).
- `description`: path of the UPnP device description on the same host and port.
- `hostname`: the machine's mDNS name, always ending in `.local`. On macOS this is the
  LocalHostName from SystemConfiguration plus `.local`; elsewhere the first label of the system
  hostname plus `.local` (what Avahi uses). Omitted if it cannot be determined. mDNS appends
  `-2` on a conflict (a second Mac with the same name, a stale record after a reboot), so a
  stored hostname can stop resolving while the Mac is up. That is accepted: the Player treats a
  hostname failure like an IP failure and moves on.
- `HEAD` is allowed. No authentication. The response carries nothing that the UPnP description
  and SSDP do not already expose on the LAN, apart from `hostname` and the extra addresses.

### Bonjour service

- Type `_spritz._tcp`, domain `local.`, port = the HTTP port.
- Instance name = the friendly name, cut to at most 63 UTF-8 bytes on a character boundary
  (the DNS label limit; a 64-character friendly name can be far longer in bytes). On a name
  conflict the responder renames automatically. The name actually registered is reported in the
  CLI's startup line and the Devices tab. TXT and identity keep the friendly name.
- TXT record: `proto=1`, `uuid=<UDN>` (with the `uuid:` prefix), `desc=/upnp/description.xml`.
- Registered when the server starts, withdrawn when it stops. Any name, port or bind change
  restarts the server and so re-registers.

### Client identification

- **SSDP:** `USER-AGENT: <os> UPnP/1.1 SpritzPlayer/<version>`. This is what the Player already
  sends; it is now specified.
- **HTTP:** a Spritz client's own requests (identity, description, SOAP, artwork) carry
  `User-Agent: SpritzPlayer/<version> (<os> <os-version>; <model>)`. `<model>` is the device
  class's marketing name ("Apple TV", later "iPhone", "iPad", "Mac"), not the hardware
  identifier (`AppleTV14,1`).
- The server treats any product token matching `Spritz[A-Za-z]+/<version>` as a Spritz client, so
  future players (`SpritzPlayer` on iOS or macOS, or another name) are recognised without a
  server change.
- Optional `X-Spritz-Device-Name: <name>` where the platform allows reading a user-visible
  device name. The server shows it in place of the model when present.
- Media streams come from AVPlayer or VLC with their own user agents. The server attributes
  them **by source IP** to a client already identified from that address. Two devices behind
  one address (a travel router, a NAT'ing hotspot) therefore merge into one record; accepted.
- **Sanitising.** Every client-supplied string (product, platform, model, device name, and the
  TXT values of the Player advertisement below) has control characters and anything outside
  printable Unicode removed, is trimmed, and is capped at 64 characters. A
  value that is empty afterwards counts as absent. The CLI prints these strings, so none may
  carry a terminal escape sequence. (hyper already rejects ESC in HTTP header values, so a raw
  escape can only arrive in the SSDP `USER-AGENT`; over HTTP the risk is C1 controls and bidi
  characters inside UTF-8. Both paths are sanitised.)

### Compatibility rules

- Adding JSON fields, TXT keys or iCloud record fields is compatible. Clients ignore unknown
  keys.
- `protocol` increments only on a breaking change. A client that sees a higher number than it
  knows still tries the `description` path.
- The DLNA surface is unchanged. Non-Spritz clients see no difference.

### iCloud rendezvous record

Written by Spritz Server only, to `NSUbiquitousKeyValueStore`.

- Store: both apps set `com.apple.developer.ubiquity-kvstore-identifier` to
  `$(TeamIdentifierPrefix)kdmf.Spritz-Server`, so they share one store. Same team, so allowed.
- Key: `spritz.server.<uuid>`, with `<uuid>` as in the identity endpoint above.
- Value: UTF-8 JSON as `Data`:

  ```json
  {
    "protocol": 1,
    "uuid": "uuid:6f1c…",
    "name": "Daniel's Mac mini",
    "hostname": "Daniels-Mac-mini.local",
    "addresses": ["192.168.1.23"],
    "port": 8080,
    "updated_at": "2026-09-26T10:15:00Z"
  }
  ```

- Written when the server starts and whenever `name`, `hostname`, `addresses` or `port` change.
  Never on a timer. `updated_at` is informational ("last published"), not liveness.
- Removed when the server stops, when the app quits, and when the user turns the setting off.
  A crash leaves the record behind; that is harmless because the Player verifies every address.
- The store holds 1 MB and 1,024 keys; one record per Mac is far inside that.
- Requires the same Apple ID on both devices, with iCloud on. A guest, a family member's Apple
  TV, a shared house, or an Apple TV whose current tvOS user has another account gets the LAN
  mechanisms only. iCloud is in addition to LAN discovery, not a replacement.
- Sync takes seconds to minutes, so this is not instant. Its value is reach, not speed.

### Player advertisement

The server cannot otherwise tell a Player blocked by the Mac's firewall from one that is simply
absent: spike S3 found that with the firewall blocking the app, the server sees no SSDP search
from the Apple TV at all — the firewall filters even the Mac's own traffic to a blocked app, so
the search never arrives to be answered or recorded. The Player advertisement gives the server a
second, independent signal that does not depend on the blocked app receiving anything.

- Type `_spritz-player._tcp`, domain `local.`, advertised by an `NWListener` that cancels every
  incoming connection.
- Instance name: the device name where readable, else the model.
- TXT: `proto=1`, `product=SpritzPlayer/<version>`, `platform=<os> <os-version>`,
  `model=<model>`.
- Advertised while the Player is looking for servers, withdrawn when it stops.
- The server browses `_spritz-player._tcp` and records each announcement under the player's
  IPv4 address only (players are keyed by IPv4 the same way HTTP clients are). This is the
  "On network" stage, alongside searched, found, browsed and streamed.

## Client stages and diagnosis

The server tracks every peer that searches, browses or streams, keyed by IP address
(IPv4-mapped IPv6 folded into IPv4):

- A search is recorded from the SSDP M-SEARCH handler, before the reply is sent — but only a
  search the server would actually answer (a relevant `ST` with `ssdp:discover`); routine
  `ssdp:all` chatter from phones, speakers and Chromecasts is not.
- Every HTTP request is mapped to a stage by path (below) and recorded from the tracking
  middleware.
- A `_spritz-player._tcp` announcement records `announced` (the Player advertisement, above)
  under the player's IPv4 address, and clears it when the advertisement is withdrawn.
- An M-SEARCH without a Spritz product token is **ignored** unless the peer is already known,
  so non-Spritz devices sending `ssdp:all` neither fill the list nor evict real clients. Any
  HTTP request lists a peer, so a Samsung TV that browses appears.
- A non-Spritz user agent never overwrites a Spritz one, so AVPlayer's `AppleCoreMedia` agent on
  a stream does not rename the Player. All strings are sanitised (above).
- **Records do not expire on idle.** At most 64 records; when full, the least recently seen is
  evicted. A Player that watched something an hour ago stays listed, greyed, rather than
  vanishing and looking like a fault.

Diagnosis is a pure function of a record and the current time, producing at most one English
sentence, keyed on `unanswered_since` (the first search with no HTTP stage within the grace
before it, cleared by any HTTP stage) rather than the latest search — keying on the latest
search would clear and re-raise the diagnosis on every one of the Player's 30 s polls:

| Condition | Text (shape) |
|-----------|--------------|
| A Spritz client whose `unanswered_since` is ≥ 15 s ago and whose latest search is ≤ 10 min ago | "{label} searched for this server but never connected. Check the firewall on this Mac (port {port})." |
| A Spritz client announced (above) for ≥ 15 s, with no HTTP stage in the 2-minute grace before the advertisement appeared | "{label} is on the network but has not connected to this server. Check the firewall on this Mac (port {port})." |
| otherwise | none |

A stuck search is checked first, so it takes precedence: a client that is both announced and
searching unanswered gets the search sentence, not the "on the network" one.

- Only Spritz clients are diagnosed. A non-Spritz peer is tracked only after it has connected,
  and TVs send routine searches without re-fetching the description, so the rule would misfire
  on them.
- The 2-minute grace before `unanswered_since` starts, and the same grace before an
  announcement, covers a lost SSDP reply or a Player that had just connected: one that connected
  a minute ago and whose next search goes unanswered, or whose HTTP stage lands just before the
  advertisement, is not reported as stuck.
- The 10-minute ceiling keeps a Player that gave up an hour ago from holding an orange
  diagnosis, and the status dot, indefinitely.
- The text names the Mac's firewall only. A search that reached the server proves the Player's
  Local Network access is on: when it is off, the platform never sends the M-SEARCH. That case
  is the Player's own to report.
- A client that fetches the description and never browses is normal (TVs probe servers they are
  not showing), so it gets no diagnosis.
- `label` is the device name, else the model, else the product, plus platform when known.

The HTTP tracking middleware maps request paths to stages:

- `/.well-known/spritz`, `/upnp/description.xml`, `/upnp/service/*`, `/spritz` (M3U) → `found`
- `/upnp/control/*`, `/upnp/event/*` → `browsed`
- `/m/*`, `/art/*` → `streamed`
- anything else → nothing recorded

A client that plays the M3U playlist streams from `/m/*` without ever browsing. It is marked
`streamed` without `browsed`, and the green-dot "connected" rule (browsed or streamed within the
last 600 s) covers it on purpose.

## Network self-checks

| id | Severity | Condition |
|----|----------|-----------|
| `no-lan-address` | Error | No LAN IPv4 address: none that is non-loopback, non-link-local, non-tunnel and non-virtual. |
| `local-network-denied` | Error | macOS: a UDP send to the subnet's gateway fails with `EHOSTUNREACH`, **and** a TCP connect to the same gateway also fails with `EHOSTUNREACH` or `EPERM` while the interface has an address. |
| `ssdp-unavailable` | Warning | UDP 1900 could not be bound. |
| `bonjour-failed` | Warning | Bonjour registration failed, synchronously or through the callback. |
| `vpn-active` | Warning | The advertised IP is on a tunnel interface. With the default bind, the advertised IP comes from the default route, so this also means "the default route goes through the tunnel". |
| `vpn-active` | Info | A tunnel interface holds an IPv4 address but is not the advertised one: "VPN interface {name} is active; Spritz advertises {ip} on {lan-if}." This keeps Tailscale, ZeroTier and split-tunnel users from a permanent orange dot. |
| `multiple-subnets` | Info | More than one LAN IPv4 subnet. The message names each and the advertised one. |

Interface classes: **tunnel** is `utun*`, `ipsec*`, `ppp*`, `tun*`, `wg*`, `feth*` (ZeroTier on
macOS), `zt*` and `tailscale*`; **virtual** is a host-only VM or container bridge: `bridge*`
(macOS), `vnic*`, `vmnet*`, `vmenet*`, `docker*`, `virbr*`, `veth*` (Linux `br*` is often the
real LAN and stays LAN). Virtual interfaces never count as LAN and never raise `vpn-active`;
`multiple-subnets` counts distinct subnets, not interfaces.

The gateway is the first host of the advertised interface's subnet (true on nearly every home
network).

The checks run at start and every **5 s**. This is one `getifaddrs` call plus, on macOS, one
UDP send, so a short interval is cheap, and plugging in Ethernet shows up within seconds without
platform-specific interface-change listeners. Each surface prints or shows only changes: the CLI
as `warning:`/`note:` lines after the startup banner, Spritz Server on its Devices tab and status
bar.

## How a Spritz player finds a server

- **SSDP burst.** The Player's existing periodic M-SEARCH burst, unchanged.
- **Bonjour browse.** An `NWBrowser` for `_spritz._tcp`. Each result is resolved with a
  short-lived `NWConnection` forced to IPv4, reading `currentPath.remoteEndpoint` for the
  address and keeping the Bonjour host name when present. A browser state of
  `.waiting(.dns(kDNSServiceErr_PolicyDenied))`, or an `NWConnection` whose path has
  `unsatisfiedReason == .localNetworkDenied`, is the Local Network denial signal.
- **iCloud records.** Every `spritz.server.*` key is read from `NSUbiquitousKeyValueStore` on
  launch, on foreground, and on each change notification, and decoded (bad records skipped).
  Each record's addresses are probed with its port through the identity endpoint. A record whose
  addresses all fail is never shown — the Player never shows a server it cannot reach, and a
  stale record is normal. A server reachable only through its iCloud addresses gets no SSDP
  replies, so it is re-probed on every burst to keep it marked online, as Bonjour and sweep hits
  are.
- **Subnet sweep.** Only when no Spritz server is currently online, and only on the first
  eligible burst after launch or foreground and then at most every 5 minutes. Only interfaces
  whose address is RFC 1918 (10/8, 172.16/12, 192.168/16); a Player on a public-address network
  or with only 169.254 addresses never sweeps. Uses the interface's real netmask, capped at a
  /22 around the Player's own address (at most 1,022 hosts), skipping the network, broadcast and
  own addresses. Priority hosts (remembered Spritz server IPs and iCloud addresses) go first,
  deduplicated, then the rest in numeric distance order from the Player's own address. Crossed
  with ports 8080, the ports of remembered Spritz servers, and the ports in iCloud records. Runs
  with at most 32 identity probes in flight, 1.5 s timeout each. Skipped entirely while Local
  Network access is denied, since unicast TCP to RFC 1918 addresses needs the same permission the
  sweep would need.
- **Fallback for a remembered server.** The stored description URL, the other stored addresses
  (same port and path) and the stored hostname are raced together under a 1.5 s deadline; the
  first description to succeed wins, and its URLs are stored. Racing means a hostname that
  cannot resolve (multicast broken) costs nothing extra, so the hostname is always included.
- All of the above — Bonjour, iCloud, sweep and SSDP — merge into one list of known servers by
  UDN (the identity `uuid`).

## Failure-mode matrix

Which mechanism survives which failure:

| Failure | SSDP | Bonjour | Sweep | Hostname | iCloud | Notes |
|---|---|---|---|---|---|---|
| Multicast filtered (IGMP snooping, some routers) | ✗ | ✗ | ✓ | ✗ | ✓ | Bonjour is multicast too. |
| Mesh Wi-Fi relaying mDNS but not other multicast (eero, Google Wifi, Orbi) | ✗ | ✓ | ✓ | ✓ | ✓ | Bonjour's main practical win. |
| DHCP changed the Mac's IP, multicast fine | ✓ | ✓ | ✓ | ✓ | ✓ | Hostname fallback matters for a server added by hand. |
| Mac and Apple TV on different subnets or VLANs | ✗ | ✗ | ✗ | ✗ | ✓ if routed | Only if the router routes between them; many IoT VLANs do not. |
| Player's Local Network access denied (iOS, iPadOS; tvOS to verify) | ✗ | ✗ | ✗ | ✗ | ✗ | The record is readable but every connection fails. The Player says so. |
| Mac firewall blocking the app | ✗ | advert ✓, connect ✗ | ✗ | ✗ | ✗ | Diagnosis from the Player advertisement (Bonjour is exempt from the firewall). A fix needs a reverse connection. |
| AP client isolation | ✗ | ✗ | ✗ | ✗ | ✗ | Only peer-to-peer Wi-Fi (future). |
| Mac asleep | ✗ | ✓ Sleep Proxy | ✓ Sleep Proxy | ✓ Sleep Proxy | ✓ Sleep Proxy | Needs the Bonjour registration (below). |

## iCloud trade-offs

- An address hint, not a liveness signal: the Player always confirms every address through the
  identity endpoint before showing a server.
- Requires the same Apple ID on both devices, with iCloud on. A guest, a family member's Apple
  TV, a shared house, or an Apple TV whose current tvOS user has another account gets the LAN
  mechanisms only.
- Not instant: key-value store sync takes seconds to minutes, and is throttled under frequent
  writes, which is why the record is written only when its contents change, never on a timer.
- What leaves the Mac: the friendly name, `.local` hostname, LAN IPv4 addresses and port — the
  same information already visible to anyone on the LAN via SSDP and the UPnP description, now
  also visible to the user's own other Apple devices through their shared iCloud account.

## Sleep Proxy

With `_spritz._tcp` registered through the system responder, an Apple TV or HomePod on the
network acts as Bonjour Sleep Proxy: it answers for the sleeping Mac and wakes it on a TCP SYN
to the registered port. It needs "Wake for network access" on in the Mac's Energy settings, and
Ethernet or a Wi-Fi chipset that supports wake. This is why a sleeping Mac is reachable on one
setup and not another, and why the Wake-on-LAN and prevent-sleep non-goals hurt less than they
look.

## Future ideas

### Short connect codes

**Problem:** typing an IP with the Siri Remote.
**Sketch:** the server shows a 2–6 character code encoding the host part and a non-default
port; the player expands it with its own subnet.
**Open questions:** subnets wider than /24; collisions across subnets.

### Phone check page

**Problem:** separating "the Mac blocks everyone" from "the TV's path is broken".
**Sketch:** `GET /spritz/check` serves a small page; Spritz Server shows a QR code for it; a
phone on the same Wi-Fi opening it proves reachability, and the Devices tab shows the hit.
**Open questions:** page content, whether the CLI prints the URL.

### Player-side "Can't find your server?" screen

**Problem:** beyond Local Network denial, the player has error context but shows nothing.
**Sketch:** after an empty burst, show the device's address, subnet and interface, whether other
devices answered SSDP (zero replies means multicast is broken here), sweep results and any
iCloud record that failed to connect ("your Mac is serving at 192.168.1.23 but this Apple TV is
on 10.0.4.7").
**Open questions:** which facts are cheap on each platform.

### Reverse connection

**Problem:** a hostile Mac firewall blocks inbound.
**Sketch:** the server connects out to the player, which runs a loopback HTTP proxy that
tunnels AVPlayer and VLC requests over that connection; the Player advertisement is the
groundwork.
**Open questions:** multiplexing, streaming throughput, battery on iOS.

### Peer-to-peer Wi-Fi

**Problem:** guest networks and client isolation.
**Sketch:** advertise and browse with `includePeerToPeer` in Network.framework (AWDL) or
MultipeerConnectivity.
**Open questions:** Apple TV support when wired, throughput for 4K, macOS sandbox behaviour.

### Wake-on-LAN and staying awake

**Problem:** a sleeping Mac disappears where no Sleep Proxy exists.
**Sketch:** a "prevent sleep while serving" option (power assertion), and players sending a
magic packet using a MAC address from the identity.
**Open questions:** exposing the MAC address; CLI flag name.

### Device names

**Problem:** every Apple TV is "Apple TV".
**Sketch:** apply for the user-assigned device name entitlement on tvOS and iOS and send
`X-Spritz-Device-Name`.
**Open questions:** Apple's approval criteria.

### Pushed events

**Problem:** the app polls the engine once a second.
**Sketch:** push check and client changes from `event_hub` through a C callback; `ConnectionHealth`
stays the published value.
**Open questions:** callback threading.

### IPv6

**Problem:** networks with broken DHCP still have link-local IPv6.
**Sketch:** add IPv6 to identity `addresses` and resolve Bonjour to IPv6 as a fallback.
**Open questions:** scope IDs in URLs.

### Status endpoint and `spritz doctor`

Considered for v1 and deferred: a `GET /spritz/status` JSON view of the checks and clients, and
a CLI subcommand that only runs the checks and listens for 30 s.
