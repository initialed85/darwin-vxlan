# darwin-vxlan

Userspace L2 VXLAN tunnel for macOS (Apple Silicon).

Bridges two hosts at the Ethernet layer over UDP using [RFC 7348](https://datatracker.ietf.org/doc/html/rfc7348) VXLAN encapsulation. Packet I/O goes through Apple's `vmnet.framework` (host-only mode), which creates a real `bridge` interface without kernel extensions or third-party drivers.

## How it works

```
Remote host                          macOS host
──────────                           ──────────
UDP :4789  ◄──── VXLAN/UDP ────►  UDP :4789
                                       │
                                  darwin-vxlan
                                       │
                               vmnet.framework
                                       │
                                  bridge100
                                  (L2 bridge)
```

- A thin C wrapper (`vmnet_bridge.c`) exposes `vmnet.framework`'s Objective-C/GCD API as plain C symbols that Rust FFI can call.
- A pipe-based notification model avoids busy-polling: vmnet signals the pipe on packet arrival, Rust calls `poll(2)` on it.
- Three concurrent tasks handle the data plane: a blocking thread drains vmnet reads, an async task forwards Ethernet frames → VXLAN → UDP, and a second async task receives UDP → VXLAN → vmnet.
- Shutdown is coordinated through a second pipe: writing one byte unblocks the `poll(2)` loop, letting the blocking thread exit cleanly before the runtime stops.

## Requirements

- macOS (Apple Silicon)
- Root privileges or the `com.apple.vm.networking` entitlement
- Rust toolchain (edition 2024)

## Build

```sh
cargo build --release
```

The build script compiles `src/vmnet_bridge.c` and links `vmnet.framework` automatically.

## Installation

### Via Homebrew (coming soon)
```bash
brew install cyborgside/tap/darwin-vxlan
```

### Manual installation
```bash
# Download the latest release
curl -L https://github.com/cyborgside/darwin-vxlan/releases/latest/download/darwin-vxlan-aarch64-apple-darwin.tar.gz | tar xz

# Move to PATH
sudo mv darwin-vxlan /usr/local/bin/

# Allow execution (Gatekeeper)
xattr -d com.apple.quarantine /usr/local/bin/darwin-vxlan

# Verify
darwin-vxlan --help
```

### Using the .pkg installer
```bash
# Download the latest .pkg
curl -LO https://github.com/cyborgside/darwin-vxlan/releases/latest/download/darwin-vxlan-*.pkg

# Install (requires sudo)
sudo installer -pkg darwin-vxlan-*.pkg -target /
```

## Usage

```
darwin-vxlan --vni <VNI> --local <IP> --remote <IP> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--vni` | _(required)_ | VXLAN Network Identifier |
| `--local` | _(required)_ | Local IP address for the VXLAN UDP socket |
| `--remote` | _(required)_ | Remote peer IP address |
| `--port` | `4789` | UDP port |
| `--mtu` | `1450` | MTU for the bridge interface |
| `--bridge-ipv4` | — | IPv4 CIDR to assign to the bridge (e.g. `192.168.100.1/24`) |
| `--bridge-ipv6` | — | IPv6 CIDR to assign to the bridge (e.g. `fd00::1/64`) |

### Example: point-to-point tunnel between two macOS hosts

**Host A** (`10.0.0.1`):
```sh
sudo darwin-vxlan --vni 100 --local 10.0.0.1 --remote 10.0.0.2 --bridge-ipv4 192.168.100.1/24
```

**Host B** (`10.0.0.2`):
```sh
sudo darwin-vxlan --vni 100 --local 10.0.0.2 --remote 10.0.0.1 --bridge-ipv4 192.168.100.2/24
```

Both sides will have a `bridge` interface with the assigned address. Traffic sent to `192.168.100.0/24` is encapsulated in VXLAN and forwarded over UDP between the two hosts.

Press `Ctrl+C` to shut down.

## Testing

Tests run without `vmnet.framework` using a pure-C stub (`src/vmnet_mock.c`) that replaces the real backend with real pipes and synthetic frames:

```sh
cargo test --features vmnet-mock
```

The stub is thread-local fault-injectable: `vmnet_mock_set_start_fail` and `vmnet_mock_set_write_fail` let individual tests trigger error paths without affecting parallel test threads.

### Coverage

```sh
cargo llvm-cov --features vmnet-mock
```

## Project structure

```
src/
  main.rs           # CLI (clap), run() / run_until() entry points
  vxlan.rs          # VxlanTunnel: constructor, run loop, packet helpers
  vmnet_bridge.c    # C wrapper around vmnet.framework
  vmnet_mock.c      # Test stub (vmnet-mock feature)
build.rs            # Selects vmnet_bridge.c vs vmnet_mock.c, links framework
```
