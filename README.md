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
- Each `darwin-vxlan` process owns one vmnet host-mode interface and one UDP socket. Three concurrent data-plane workers run inside that process: a blocking thread drains vmnet reads, an async task forwards Ethernet frames → VXLAN → UDP, and a second async task receives UDP → VXLAN → vmnet.
- Shutdown is coordinated through a second pipe and an async cancellation signal: writing one byte unblocks the `poll(2)` loop, and all forwarding workers are joined before the vmnet context is dropped.

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

### Via Homebrew
A Homebrew formula is not currently published for this fork.

### Manual installation
```bash
# Download the latest release
curl -L https://github.com/initialed85/darwin-vxlan/releases/latest/download/darwin-vxlan-aarch64-apple-darwin.tar.gz | tar xz

# Move to PATH
sudo mv darwin-vxlan /usr/local/bin/

# Allow execution (Gatekeeper)
xattr -d com.apple.quarantine /usr/local/bin/darwin-vxlan

# Verify
darwin-vxlan --help
```

### Using the .pkg installer
```bash
# Set this to the release version you want to install.
VERSION=0.1.0
curl -LO "https://github.com/initialed85/darwin-vxlan/releases/download/v${VERSION}/darwin-vxlan-${VERSION}.pkg"

# Install (requires sudo)
sudo installer -pkg "darwin-vxlan-${VERSION}.pkg" -target /
```

## Releases

The release workflow builds the Apple Silicon tarball and `.pkg` installer when a
`v*` tag is pushed. To publish a release from this fork:

```sh
git tag v0.1.0
git push origin v0.1.0
```

A release can also be started manually from the **Release** workflow by
supplying a version such as `0.1.0`; the workflow creates the corresponding
`v0.1.0` release tag.

## Usage

```
darwin-vxlan --vni <VNI> --local <IP> --remote <IP> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--vni` | _(required)_ | VXLAN Network Identifier |
| `--local` | _(required)_ | Local IP address for the VXLAN UDP socket |
| `--remote` | _(required)_ | Remote peer IP address |
| `--port` | `4789` | UDP port used for both the local VXLAN listen/bind and the remote peer destination; use `8472` for K3s Flannel |
| `--mtu` | `1450` | MTU for the bridge interface |
| `--bridge-ipv4` | — | IPv4 CIDR to assign to the bridge (e.g. `192.168.100.1/24`) |
| `--bridge-ipv6` | — | IPv6 CIDR to assign to the bridge (e.g. `fd00::1/64`) |

### Bridge naming

macOS/vmnet does not expose a supported API for choosing the host-mode
interface name. This experiment works around that by reserving the unused
bridge units between `bridge100` and the requested VNI before starting vmnet.
As a result:

```text
--vni 199  ->  bridge199
--vni 137  ->  bridge137
```

The temporary bridge devices are destroyed after vmnet starts. The requested
bridge must not already exist, and bridge allocation must not race another
program creating a bridge. This requires the same privileges as vmnet (run as
root or use the vmnet entitlement). For VNIs below 100, vmnet's
allocator-selected bridge name is accepted because it cannot be coupled to
`bridge<VNI>`; the VNI still remains in every VXLAN header. The current
allocator hack also refuses VNIs above 4095 rather than creating thousands of
temporary bridge devices.

### Example: point-to-point tunnel between two macOS hosts

**Host A** (`10.0.0.1`):
```sh
sudo darwin-vxlan --vni 100 --local 10.0.0.1 --remote 10.0.0.2 --bridge-ipv4 192.168.100.1/24
```

**Host B** (`10.0.0.2`):
```sh
sudo darwin-vxlan --vni 100 --local 10.0.0.2 --remote 10.0.0.1 --bridge-ipv4 192.168.100.2/24
```

With `--vni 100`, both sides will have a `bridge100` interface with the assigned address. Traffic sent to `192.168.100.0/24` is encapsulated in VXLAN and forwarded over UDP between the two hosts.

Press `Ctrl+C` to shut down.

### Example: interoperate with K3s Flannel

K3s Flannel's VXLAN backend uses VNI `1` and UDP port `8472` (rather than the
standard VXLAN port `4789`). `--port` controls both the local UDP bind and the
remote destination, so set it to `8472` for this interop:

```sh
sudo darwin-vxlan \
  --vni 1 \
  --local 192.168.1.128 \
  --remote 192.168.1.111 \
  --port 8472 \
  --bridge-ipv4 10.42.0.250/16
```

Use an address and bridge CIDR appropriate for the macOS host and the Flannel
node. The VNI is carried in every VXLAN header; for VNI `1`, vmnet uses its
allocator-selected bridge name because host-mode bridge numbering starts at
`bridge100`.

### Process concurrency

A process is currently a single point-to-point VXLAN endpoint: it has one
`--remote` destination and one local UDP bind. Running another process with the
same local address and UDP port will fail with an address-in-use error. Multiple
processes are possible only when they use distinct local bind addresses/ports,
and vmnet bridge allocation must not race between processes. To reach multiple
Flannel peers, use a design that provides peer fan-out rather than starting
several processes on the same `--port 8472`.

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
