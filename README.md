# microdns

Micro-daemon that advertises mDNS/DNS-SD services for BigFred OS.

Quietly retries when interfaces, the microinit control socket, or dcc-bus are
unavailable. Always starts successfully.

## Features

- Static DNS-SD services from `$DATA_DIR/etc/microdns.json` (default `/data`)
- Optional dcc-bus discovery: when enabled, watches microinit for a running
  `dcc-bus` process and advertises `_z21._udp` / `_withrottle._tcp` only when
  those ports are empirically listening
- Optional Z21 UDP LAN discovery beacon (LAN_GET_SERIAL_NUMBER reply broadcast)
- Hot-reload via inotify on the config file
- Static musl builds for linux/arm64 and linux/amd64

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
  }
}
```

- `dccBus.enabled` (default `false`): when false, only static `services[]` are advertised.
- Retry intervals are configurable; config changes are hot-reloaded.

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
