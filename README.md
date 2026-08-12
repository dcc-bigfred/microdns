# microdns

Micro-daemon that advertises mDNS/DNS-SD services for BigFred OS.

Quietly retries when interfaces, the microinit control socket, or dcc-bus are
unavailable. Always starts successfully. Survives network drop/return and
interface add/remove via rtnetlink (with polling fallback). Resolves hostnames
per receiving interface so a WiFi client gets the WiFi address.

## Features

- Static DNS-SD services from `$DATA_DIR/etc/microdns.json` (default `/data`)
- Hostname A records for configured `host` values (e.g. `bigfred` → `bigfred.local`)
- Own legacy unicast / one-shot mDNS responder (RFC 6762 §6.7) so browsers and OS
  resolvers (Android `getaddrinfo`) can resolve `.local` names — not only DNS-SD
  browsers. Needed because **mdns-sd 0.20.3** answers unicast but hardcodes
  transaction ID=`0` (`dns_parser.rs`: `let id = if self.multicast { 0 } else { self.id }`
  while `DnsOutgoing.multicast` is never set false). Remove `legacy_unicast` when
  upstream fixes that encoding.
- Optional dcc-bus discovery: when enabled, watches microinit for a running
  `dcc-bus` process and advertises `_z21._udp` / `_withrottle._tcp` only when
  those ports are empirically listening
- Optional Z21 UDP LAN discovery beacon (LAN_GET_SERIAL_NUMBER reply broadcast)
- Hot-reload via inotify on the config file
- Static musl builds for linux/arm64 and linux/amd64

## Tests

Integration-style unit tests live under `tests/` (one file per module), matching
the microinit layout. Run with `cargo test` / `make test`.

## Config

Default path: `$DATA_DIR/etc/microdns.json`. Created with defaults if missing.

```json
{
  "services": [
    {
      "name": "bigfred",
      "type": "_http._tcp",
      "protocol": "tcp",
      "port": 8080,
      "host": "bigfred",
      "txt": { "path": "/" }
    }
  ],
  "dccBus": {
    "enabled": false,
    "z21Port": 21105,
    "withrottlePort": 12090,
    "beacon": true
  },
  "retry": {
    "microinitMs": 2000,
    "procMs": 2000,
    "mdnsMs": 3000,
    "ifaceMs": 5000
  },
  "skipInterfaces": [],
  "interfaces": []
}
```

- `dccBus.enabled` (default `false`): when false, only static `services[]` are advertised.
- Retry intervals are configurable; config changes are hot-reloaded.
- `skipInterfaces` (default `[]`): extra interface-name prefixes to skip
  (case-insensitive), in addition to the built-in docker/veth/br-*/cni/
  flannel/virbr list. Empty by default so mDNS advertises on every usable
  interface, including `wlan*` (a laptop on WiFi). Add `["wlan"]` on a hub
  that reserves the WiFi radio for another purpose (e.g. the BigFred hub,
  where `wireless-programmer` owns the radio) so mDNS does not leak
  `bigfred.local` / dcc-bus beacons onto a device config network.
  Entries are name **prefixes**, not globs or exact names: `"wlan"` covers
  `wlan0`/`wlan1` but not `wlp3s0`, and a short entry like `"e"` would take
  `eth0` and `enp1s0` with it, leaving nothing to advertise on.
- `interfaces` (default `[]`): optional allowlist of interface-name prefixes
  (same prefix rules as `skipInterfaces`). Empty means use every usable
  interface that is not skipped. When set (e.g. `["eth","enp"]`), only
  matching interfaces are used; a listed interface that disappears logs a
  warning and is retried — it does not crash the daemon.
- Hostname A/AAAA answers (`bigfred.local`) are selected **per receiving
  interface** (via `IP_PKTINFO`): a client querying on WiFi gets the WiFi
  address, not the Ethernet one. Interface add/remove/address changes are
  detected via rtnetlink with polling fallback.

## Run

```bash
microdns serve
# or
microdns run
# or just
microdns
```

Flags:

- `--config <path>` — config file (default `$DATA_DIR/etc/microdns.json`)
- `--data-dir <path>` — set `DATA_DIR` before start
- `--version` / `info` — build and release metadata

## Build

```bash
export RUSTUP_TOOLCHAIN=stable
make build
make release
make release-musl   # aarch64-unknown-linux-musl → dist/microdns-linux-arm64
make test
make clippy
```

## License

MIT
