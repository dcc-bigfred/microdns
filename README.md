# microdns

Micro-daemon that advertises mDNS/DNS-SD services for BigFred OS.

Quietly retries when interfaces, the BigFred control socket, or the microinit
watch socket are unavailable.
Always starts successfully. Survives network drop/return and
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
- Optional dcc-bus discovery: when `bigfred.enabled` (default true), polls the
  loco-server Unix socket (`$DATA_DIR/run/bigfred.sock`) for `dcc_bus_list` and
  advertises `_z21._udp` / `_withrottle._tcp` on the ports in that JSON. Missing
  socket is retried with exponential backoff (2 s … `retry.bigfredMs`, default
  45 s). Last-good dcc-bus ads are kept across a short socket outage and only
  withdrawn after `retry.bigfredMs` of consecutive failures.
- Optional microinit watch: when `microinit.enabled` (default true), holds one
  connection to `$DATA_DIR/run/microinit.sock` (`{type:watch,label_keys:["microdns-port"]}`)
  and advertises running services that have `microdns-port` + `microdns-type`.
  `microdns-host` is optional (kernel hostname if omitted). `microdns-txt-*`
  labels become TXT pairs. Reconnect backoff is `retry.microinitReconnectMs`
  (default 3s). Last-good ads are kept across a dropped socket.
- Periodic unsolicited re-announcements (`announce.periodMs`, default 55 s) plus
  a 1/2/4/8 s burst after a real advertisement change. Re-register never sends
  a goodbye (TTL 0); `unregister` is only used when a service is actually gone.
- Self-check every `selfcheck.periodMs` (default 60 s): IGMP 224.0.0.251 on
  used NICs, mdns-sd thread alive, recent `Announce` events. Escalates
  re-announce → daemon recreate.
- Optional Z21 UDP LAN discovery beacon (LAN_GET_SERIAL_NUMBER reply broadcast)
- Unix control socket (`$DATA_DIR/run/microdns.sock`): `microdns services list`
  and `microdns doctor` query the live daemon
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
  "bigfred": { "enabled": true },
  "microinit": { "enabled": true },
  "dccBus": {
    "beacon": true,
    "host": "bigfred"
  },
  "retry": {
    "bigfredMs": 45000,
    "pollMs": 25000,
    "mdnsMs": 3000,
    "ifaceMs": 5000,
    "microinitReconnectMs": 3000
  },
  "announce": { "periodMs": 55000, "burstCount": 4 },
  "selfcheck": { "periodMs": 60000 },
  "skipInterfaces": [],
  "interfaces": []
}
```

- `bigfred.enabled` (default `true`): poll loco-server for dcc-bus programs.
  Set `false` to skip dcc-bus ads.
- `microinit.enabled` (default `true`): watch microinit for labeled services.
  Set `false` if this host has no microinit socket.
- `dccBus.beacon` (default `true`): Z21 LAN serial broadcast on advertised UDP ports.
- `dccBus.host` (optional): DNS-SD hostname without `.local` for `_z21._udp` /
  `_withrottle._tcp` ads. When omitted, mdns-sd uses the kernel hostname and
  `microdns services list` shows `-` in HOST. Product templates set `"bigfred"`.
- `retry.bigfredMs` (default `45000`): cap on backoff while the BigFred socket
  is down, and grace period before withdrawing last-good dcc-bus ads.
  First failures retry at 2 s, 4 s, 8 s, … up to this cap.
  `retry.pollMs` (default `25000`) is the poll interval once connected.
  Existing files may still use `retry.microinitMs`; that alias still maps to
  `pollMs` (BigFred), **not** the microinit watch. Use `retry.microinitReconnectMs`
  for watch reconnect backoff.
- `announce.periodMs` (default `55000`): unsolicited re-announce interval, kept
  below the 120 s host-record TTL. `announce.burstCount` (default `4`) extra
  announcements at 1 s, 2 s, 4 s, 8 s after a real change.
- `selfcheck.periodMs` (default `60000`): how often to verify multicast
  membership and recent announcements.
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
  detected via rtnetlink with polling fallback. Netlink events on skipped
  interfaces (e.g. `wlan0` on the hub) do not re-announce Ethernet mDNS.
  IGMP leave+join is reserved for real address-set changes and suspend/resume.
- On a BigFred hub, `micronet` in gateway mode re-probes foreign DHCP every 15 s
  and a failed `micronet check` tears the address down. microdns treats that as
  a normal address change (re-announce, no goodbye).

## Run

```bash
microdns serve
# or
microdns run
# or just
microdns
```

List what a **running** daemon is advertising (static `services[]` plus dynamic
`_z21._udp` / `_withrottle._tcp` from the last `dcc_bus_list` and microinit
label watch; not Z21 LAN beacons):

```bash
microdns services list
microdns services list -o json
microdns services list --socket /data/run/microdns.sock
```

Human columns: `NAME`, `TYPE`, `PROTO`, `PORT`, `HOST`, `SOURCE` (`static`, `dccBus`, or `microinit`).
JSON is a pretty-printed `{ "services": [ ... ] }` with the same rows (camelCase,
plus optional `host` / `txt`). The CLI talks to the live daemon over the ctl
socket — it does not read `microdns.json` on its own. If the socket is missing,
the error is the same shape as `bf` (`is microdns running?`).

Diagnose sockets, IGMP membership, mdns-sd counters, and the last self-check
(works even if the daemon is down for the local kernel half):

```bash
microdns doctor
microdns doctor -o json
```

On a second machine in the same LAN, confirm there are no goodbye packets
(TTL 0) except when a service is actually removed, and that unsolicited
announcements repeat about every 55 s:

```bash
tcpdump -ni <iface> -vv 'udp port 5353 and host <hub-ip>'
# look for: bigfred._http._tcp.local, TTL 0 (bad unless the service went away)
microinit logs microdns --follow   # on the hub
cat /proc/net/igmp                 # 224.0.0.251 on eth0
```

Flags:

- `--config <path>` — config file (default `$DATA_DIR/etc/microdns.json`)
- `--data-dir <path>` — set `DATA_DIR` before start
- `--socket <path>` — ctl socket (default `$DATA_DIR/run/microdns.sock`)
- `-o, --output human|json` — `services list` / `doctor` output (default `human`)
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
