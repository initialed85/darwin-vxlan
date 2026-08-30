use anyhow::Result;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

const VXLAN_MAX_VNI: u32 = 0x00ff_ffff;
/// vmnet's host-mode bridge allocator starts at bridge100 on macOS. Keeping
/// every lower unit occupied makes the next vmnet bridge deterministic.
#[cfg(not(feature = "vmnet-mock"))]
const VMNET_BRIDGE_BASE: u32 = 100;
/// Creating thousands of temporary bridge devices is a bad failure mode for a
/// naming convenience. Raise this only if the allocator hack proves useful
/// beyond the small VNIs used by this experiment.
#[cfg(not(feature = "vmnet-mock"))]
const VMNET_MAX_FORCED_VNI: u32 = 4095;
use std::time::Duration;

const VXLAN_HEADER_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// vmnet C FFI
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn vmnet_ctx_start(requested_mtu: u32) -> *mut libc::c_void;
    fn vmnet_ctx_notify_fd(ctx: *mut libc::c_void) -> libc::c_int;
    fn vmnet_ctx_shutdown_write_fd(ctx: *mut libc::c_void) -> libc::c_int;
    fn vmnet_ctx_shutdown_read_fd(ctx: *mut libc::c_void) -> libc::c_int;
    fn vmnet_ctx_mtu(ctx: *mut libc::c_void) -> u32;
    fn vmnet_ctx_max_packet_size(ctx: *mut libc::c_void) -> libc::size_t;
    fn vmnet_ctx_read_one(ctx: *mut libc::c_void, buf: *mut u8, len: libc::size_t) -> libc::c_int;
    fn vmnet_ctx_write(ctx: *mut libc::c_void, buf: *const u8, len: libc::size_t) -> libc::c_int;
    fn vmnet_ctx_stop(ctx: *mut libc::c_void);
}


#[cfg(feature = "vmnet-mock")]
unsafe extern "C" {
    fn vmnet_mock_set_start_fail(v: libc::c_int);
    fn vmnet_mock_set_write_fail(v: libc::c_int);
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

pub struct VxlanTunnel {
    vni: u32,
    remote_addrs: Vec<SocketAddr>,
    socket: Arc<UdpSocket>,
    /// Opaque pointer to heap-allocated vmnet_ctx_t.
    ctx: *mut libc::c_void,
    notify_fd: i32,
    shutdown_write_fd: i32,
    shutdown_read_fd: i32,
    max_packet_size: usize,
    bridge_name: String,
}

// vmnet_ctx_t is heap-allocated and vmnet_read/write use internal locking.
unsafe impl Send for VxlanTunnel {}
unsafe impl Sync for VxlanTunnel {}

impl Drop for VxlanTunnel {
    fn drop(&mut self) {
        unsafe { vmnet_ctx_stop(self.ctx); }
    }
}

/// Cleans up a vmnet context if construction fails after vmnet_start succeeds.
/// The old code leaked the context on bridge/address setup errors; this guard
/// also matters now that bridge-number reservations can fail part-way through.
struct VmnetContextGuard {
    ctx: *mut libc::c_void,
}

impl VmnetContextGuard {
    fn new(ctx: *mut libc::c_void) -> Self {
        Self { ctx }
    }

    fn disarm(&mut self) {
        self.ctx = std::ptr::null_mut();
    }
}

impl Drop for VmnetContextGuard {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { vmnet_ctx_stop(self.ctx); }
        }
    }
}

struct BridgeReservations {
    names: Vec<String>,
}

impl BridgeReservations {
    fn empty() -> Self {
        Self { names: Vec::new() }
    }
}

impl Drop for BridgeReservations {
    fn drop(&mut self) {
        for name in self.names.drain(..).rev() {
            let status = std::process::Command::new("ifconfig")
                .args([&name, "destroy"])
                .status();
            match status {
                Ok(status) if status.success() => {}
                Ok(status) => tracing::warn!(
                    "failed to destroy temporary bridge {} (exit {})",
                    name,
                    status
                ),
                Err(error) => tracing::warn!(
                    "failed to destroy temporary bridge {}: {}",
                    name,
                    error
                ),
            }
        }
    }
}

fn bridge_name_for_vni(vni: u32) -> Result<String> {
    if vni > VXLAN_MAX_VNI {
        anyhow::bail!("VNI {} is outside the 24-bit VXLAN range", vni);
    }
    Ok(format!("bridge{}", vni))
}

/// Reserve bridge units below the requested VNI where vmnet's allocator can
/// be coupled to the VNI. vmnet.framework does not provide an interface-name
/// parameter; on macOS host-mode vmnet allocates bridge units starting at
/// bridge100. For VNIs below bridge100, use the allocator-selected bridge and
/// keep the requested VNI only in the VXLAN header.
#[cfg(not(feature = "vmnet-mock"))]
fn reserve_bridge_units(vni: u32) -> Result<BridgeReservations> {
    if vni < VMNET_BRIDGE_BASE {
        return Ok(BridgeReservations::empty());
    }
    if vni > VMNET_MAX_FORCED_VNI {
        anyhow::bail!(
            "cannot force bridge{}: refusing to create more than {} temporary bridge devices",
            vni,
            VMNET_MAX_FORCED_VNI - VMNET_BRIDGE_BASE
        );
    }

    let desired = bridge_name_for_vni(vni)?;
    let existing = list_bridge_interfaces();
    if existing.iter().any(|name| name == &desired) {
        anyhow::bail!(
            "{} already exists; refusing to use or disturb an existing bridge",
            desired
        );
    }

    let mut reservations = BridgeReservations::empty();
    for unit in VMNET_BRIDGE_BASE..vni {
        let name = format!("bridge{}", unit);
        if existing.iter().any(|current| current == &name) {
            continue;
        }
        let status = std::process::Command::new("ifconfig")
            .args([&name, "create"])
            .status()
            .map_err(|error| anyhow::anyhow!("create temporary {}: {}", name, error))?;
        if !status.success() {
            anyhow::bail!("failed to create temporary {} (exit {})", name, status);
        }
        reservations.names.push(name);
    }
    Ok(reservations)
}

// ---------------------------------------------------------------------------
// Test helpers (mock build only)
// ---------------------------------------------------------------------------

#[cfg(feature = "vmnet-mock")]
impl VxlanTunnel {
    /// Returns the local address the UDP socket is bound to.
    /// Used in tests to determine the OS-assigned port when binding to port 0.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
    }
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

impl VxlanTunnel {
    /// Construct a point-to-point tunnel using the same UDP port for the
    /// local bind and its single remote peer. Kept as a compatibility wrapper
    /// for callers that do not need peer fan-out.
    pub async fn new(
        vni: u32,
        local: IpAddr,
        remote: IpAddr,
        port: u16,
        mtu: u32,
        bridge_ipv4: Option<&str>,
        bridge_ipv6: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_remotes(
            vni,
            SocketAddr::new(local, port),
            vec![SocketAddr::new(remote, port)],
            mtu,
            bridge_ipv4,
            bridge_ipv6,
        ).await
    }

    /// Construct a tunnel with one local UDP socket and one or more remote
    /// VTEP endpoints. Ethernet frames read from vmnet are sent to every
    /// configured remote; incoming VXLAN frames are accepted on the shared
    /// socket and still filtered by the requested VNI.
    pub async fn new_with_remotes(
        vni: u32,
        local_addr: SocketAddr,
        remote_addrs: Vec<SocketAddr>,
        mtu: u32,
        bridge_ipv4: Option<&str>,
        bridge_ipv6: Option<&str>,
    ) -> Result<Self> {
        if remote_addrs.is_empty() {
            anyhow::bail!("at least one remote VTEP endpoint is required");
        }

        let desired_bridge = bridge_name_for_vni(vni)?;
        let socket = UdpSocket::bind(local_addr).await
            .map_err(|e| anyhow::anyhow!("Failed to bind UDP socket: {}", e))?;
        #[cfg(feature = "vmnet-mock")]
        let _ = &desired_bridge;

        // macOS does not expose a supported vmnet option for choosing the
        // bridge name. Reserve the lower bridge units so vmnet's next
        // automatically-created interface is bridge<VNI> where possible. This
        // is deliberately a small experiment-specific hack; reservations are
        // removed on every exit path by BridgeReservations' Drop implementation.
        #[cfg(not(feature = "vmnet-mock"))]
        let _bridge_reservations = reserve_bridge_units(vni)?;
        #[cfg(feature = "vmnet-mock")]
        let _bridge_reservations = BridgeReservations::empty();

        let bridges_before = list_bridge_interfaces();

        let ctx = unsafe { vmnet_ctx_start(mtu) };
        if ctx.is_null() {
            anyhow::bail!(
                "vmnet_ctx_start() failed — run as root. \
                 vmnet.framework requires root or the com.apple.vm.networking entitlement."
            );
        }
        let mut ctx_guard = VmnetContextGuard::new(ctx);

        let (notify_fd, shutdown_write_fd, shutdown_read_fd, max_pkt, vmnet_mtu) = unsafe {(
            vmnet_ctx_notify_fd(ctx),
            vmnet_ctx_shutdown_write_fd(ctx),
            vmnet_ctx_shutdown_read_fd(ctx),
            vmnet_ctx_max_packet_size(ctx),
            vmnet_ctx_mtu(ctx),
        )};

        // Retry until macOS registers the new bridge (up to 3 s).
        let bridge = wait_for_new_bridge(&bridges_before)?;

        #[cfg(not(feature = "vmnet-mock"))]
        if vni >= VMNET_BRIDGE_BASE && bridge != desired_bridge {
            anyhow::bail!(
                "vmnet created {}, expected {} for VNI {}; another interface may have raced the bridge allocator",
                bridge, desired_bridge, vni
            );
        }

        // Remove the IPv4 that vmnet auto-assigns.
        remove_all_inet4(&bridge);

        // Optionally assign user-specified addresses.
        if let Some(cidr) = bridge_ipv4 {
            assign_inet4(&bridge, cidr)?;
        }
        if let Some(cidr) = bridge_ipv6 {
            assign_inet6(&bridge, cidr)?;
        }

        tracing::info!(
            "vmnet bridge: {} | mtu={} | max_pkt={}",
            bridge, vmnet_mtu, max_pkt
        );
        tracing::info!(
            "VXLAN ready: local={} remotes={:?} vni={}",
            local_addr, remote_addrs, vni
        );

        let tunnel = Self {
            vni,
            remote_addrs,
            socket: Arc::new(socket),
            ctx,
            notify_fd,
            shutdown_write_fd,
            shutdown_read_fd,
            max_packet_size: max_pkt,
            bridge_name: bridge,
        };
        ctx_guard.disarm();
        Ok(tunnel)
    }
}

// ---------------------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------------------

impl VxlanTunnel {
    /// Run the tunnel until `shutdown` resolves.
    ///
    /// In production use `run()`, which passes `tokio::signal::ctrl_c()`.
    /// Tests pass a short timer so they never touch SIGINT.
    pub async fn run_until<F>(&self, shutdown: F) -> Result<()>
    where
        F: std::future::Future<Output = std::io::Result<()>>,
    {
        let vni               = self.vni;
        let remote_addrs      = self.remote_addrs.clone();
        let notify_fd         = self.notify_fd;
        let shutdown_write_fd = self.shutdown_write_fd;
        let shutdown_read_fd  = self.shutdown_read_fd;
        let max_pkt           = self.max_packet_size;
        let ctx_read          = self.ctx as usize; // usize is Send; recast inside thread
        let ctx_write         = self.ctx as usize;
        let socket_tx         = self.socket.clone();
        let socket_rx         = self.socket.clone();
        let bridge_name       = self.bridge_name.clone();

        let (eth_tx, mut eth_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        // The forwarding tasks must stop before VxlanTunnel is dropped. In
        // particular, the UDP receive task holds the vmnet context pointer and
        // would otherwise outlive the context after run_until() returns.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // vmnet → channel  (blocking thread: poll + vmnet_read)
        let blocking_handle = tokio::task::spawn_blocking(move || {
            let ctx = ctx_read as *mut libc::c_void;
            let mut buf = vec![0u8; max_pkt];
            loop {
                let mut pfds = [
                    libc::pollfd { fd: notify_fd,        events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: shutdown_read_fd, events: libc::POLLIN, revents: 0 },
                ];
                if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } <= 0 { break; }

                // Shutdown fd readable → exit cleanly.
                if pfds[1].revents & libc::POLLIN != 0 { break; }

                // Drain notification pipe.
                let mut drain = [0u8; 256];
                loop {
                    let r = unsafe {
                        libc::read(notify_fd,
                            drain.as_mut_ptr() as *mut libc::c_void,
                            drain.len() as libc::size_t)
                    };
                    if r <= 0 { break; }
                }

                // Read all available frames.
                loop {
                    let n = unsafe {
                        vmnet_ctx_read_one(ctx, buf.as_mut_ptr(), buf.len() as libc::size_t)
                    };
                    if n <= 0 { break; }
                    let frame = buf[..n as usize].to_vec();
                    if eth_tx.send(frame).is_err() { return; }
                }
            }
        });

        // channel → VXLAN → UDP  (async)
        let tx_shutdown = shutdown_rx.clone();
        let tx_handle = tokio::spawn(async move {
            let mut shutdown = tx_shutdown;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    maybe_frame = eth_rx.recv() => {
                        let Some(eth_frame) = maybe_frame else { break };
                        tracing::debug!("vmnet→vxlan: {} bytes", eth_frame.len());
                        let vxlan = build_vxlan_payload(&eth_frame, vni);
                        for remote_addr in &remote_addrs {
                            socket_tx.send_to(&vxlan, remote_addr).await.ok();
                        }
                    }
                }
            }
        });

        // UDP → VXLAN → vmnet  (async recv + sync vmnet_write)
        let rx_handle = tokio::spawn(async move {
            let mut shutdown = shutdown_rx;
            let mut buf = vec![0u8; 65535];
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    result = socket_rx.recv_from(&mut buf) => {
                        match result {
                            Ok((len, src)) => {
                                tracing::debug!("recv {} bytes from {}", len, src);
                                // Recast after the await — *mut c_void must not cross await points.
                                let ctx = ctx_write as *mut libc::c_void;
                                vxlan_to_vmnet(&buf[..len], vni, ctx, &bridge_name);
                            }
                            Err(e) => {
                                tracing::error!("UDP recv: {}", e);
                                tokio::select! {
                                    _ = shutdown.changed() => break,
                                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                                }
                            }
                        }
                    }
                }
            }
        });

        tracing::info!("Tunnel running. Press Ctrl+C to stop.");
        // Always tear down the forwarding tasks, including when the shutdown
        // future itself returns an error, before the vmnet context is dropped.
        let shutdown_result = shutdown.await;
        tracing::info!("Shutting down.");
        let _ = shutdown_tx.send(true);

        // Unblock the poll loop in the blocking thread so the runtime can finish.
        unsafe { libc::write(shutdown_write_fd, [1u8].as_ptr() as *const libc::c_void, 1); }
        let _ = blocking_handle.await;
        let _ = tx_handle.await;
        let _ = rx_handle.await;

        shutdown_result?;
        Ok(())
    }

    /// Run the tunnel until Ctrl+C (SIGINT).
    #[cfg(test)]
    pub async fn run(&self) -> Result<()> {
        self.run_until(tokio::signal::ctrl_c()).await
    }
}

// ---------------------------------------------------------------------------
// Packet helpers
// ---------------------------------------------------------------------------

/// Prepend an 8-byte VXLAN header (RFC 7348) to a raw Ethernet frame.
fn build_vxlan_payload(eth: &[u8], vni: u32) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(VXLAN_HEADER_SIZE + eth.len());
    let vni_be = vni.to_be_bytes(); // [0, hi, mid, lo]
    pkt.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // flags: I-bit set, reserved
    pkt.extend_from_slice(&vni_be[1..]);               // 3-byte VNI (big-endian)
    pkt.push(0x00);                                     // reserved
    pkt.extend_from_slice(eth);
    pkt
}

/// Strip the VXLAN header and inject the inner Ethernet frame into vmnet.
fn vxlan_to_vmnet(frame: &[u8], vni: u32, ctx: *mut libc::c_void, bridge: &str) {
    let Some(eth) = unwrap_vxlan(frame, vni) else { return };
    let ret = unsafe { vmnet_ctx_write(ctx, eth.as_ptr(), eth.len() as libc::size_t) };
    if ret < 0 {
        tracing::warn!("vmnet_write to {} failed", bridge);
    }
}

/// Validate VXLAN header and return the inner Ethernet frame, or None.
fn unwrap_vxlan(frame: &[u8], vni: u32) -> Option<&[u8]> {
    if frame.len() < VXLAN_HEADER_SIZE {
        tracing::warn!("VXLAN frame too short ({} bytes)", frame.len());
        return None;
    }
    let recv_vni = u32::from_be_bytes([0, frame[4], frame[5], frame[6]]);
    if recv_vni != vni {
        tracing::warn!("VNI mismatch: expected {}, got {}", vni, recv_vni);
        return None;
    }
    Some(&frame[VXLAN_HEADER_SIZE..])
}

// ---------------------------------------------------------------------------
// Bridge helpers
// ---------------------------------------------------------------------------

fn list_bridge_interfaces() -> Vec<String> {
    let Ok(out) = std::process::Command::new("ifconfig").arg("-a").output() else { return vec![] };
    let Ok(text) = std::str::from_utf8(&out.stdout) else { return vec![] };
    parse_bridge_names(text)
}

fn parse_bridge_names(ifconfig_output: &str) -> Vec<String> {
    ifconfig_output.lines()
        .filter(|l| !l.starts_with('\t') && !l.starts_with(' '))
        .filter_map(|l| {
            let name = l.split(':').next()?.trim().to_string();
            if name.starts_with("bridge") { Some(name) } else { None }
        })
        .collect()
}

/// Real implementation: poll `ifconfig -a` every 100 ms for up to 3 s.
#[cfg(not(feature = "vmnet-mock"))]
fn wait_for_new_bridge(before: &[String]) -> Result<String> {
    (0..30)
        .find_map(|i| {
            if i > 0 { std::thread::sleep(Duration::from_millis(100)); }
            find_new_bridge(before)
        })
        .ok_or_else(|| anyhow::anyhow!(
            "Bridge interface not found after 3 s. Run `ifconfig -a` to inspect."
        ))
}

/// Mock implementation: return immediately with a synthetic bridge name.
#[cfg(feature = "vmnet-mock")]
fn wait_for_new_bridge(_before: &[String]) -> Result<String> {
    Ok("mock_bridge0".to_string())
}

fn find_new_bridge(before: &[String]) -> Option<String> {
    find_new_bridge_in(&list_bridge_interfaces(), before)
}

fn find_new_bridge_in(current: &[String], before: &[String]) -> Option<String> {
    let before_set: std::collections::HashSet<&str> = before.iter().map(String::as_str).collect();
    current.iter().find(|b| !before_set.contains(b.as_str())).cloned()
}

/// Remove all IPv4 addresses from an interface.
fn remove_all_inet4(iface: &str) {
    let Ok(out) = std::process::Command::new("ifconfig").arg(iface).output() else { return };
    let Ok(text) = std::str::from_utf8(&out.stdout) else { return };
    remove_inet4_addrs(iface, text);
}

/// Delete each address listed in `ifconfig_output` from `iface`.
fn remove_inet4_addrs(iface: &str, ifconfig_output: &str) {
    for addr in parse_inet4_addrs(ifconfig_output) {
        let _ = std::process::Command::new("ifconfig")
            .args([iface, "inet", &addr, "delete"])
            .status();
    }
}

fn parse_inet4_addrs(ifconfig_output: &str) -> Vec<String> {
    ifconfig_output.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("inet ")?;
            let addr = rest.split_whitespace().next()?;
            if addr.is_empty() { None } else { Some(addr.to_string()) }
        })
        .collect()
}

/// Assign an IPv4 address in CIDR notation (e.g. "192.168.100.1/24") to an interface.
fn assign_inet4(iface: &str, cidr: &str) -> Result<()> {
    let status = std::process::Command::new("ifconfig")
        .args([iface, "inet", cidr, "alias"])
        .status()
        .map_err(|e| anyhow::anyhow!("ifconfig inet failed: {}", e))?;
    if !status.success() {
        anyhow::bail!("Failed to assign IPv4 {} to {}", cidr, iface);
    }
    tracing::info!("bridge {} inet  {}", iface, cidr);
    Ok(())
}

fn parse_cidr(cidr: &str) -> Result<(&str, &str)> {
    cidr.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("'{}' is not valid CIDR notation (expected addr/prefixlen)", cidr)
    })
}

/// Assign an IPv6 address in CIDR notation (e.g. "fd00::1/64") to an interface.
fn assign_inet6(iface: &str, cidr: &str) -> Result<()> {
    let (addr, prefixlen) = parse_cidr(cidr)?;
    let status = std::process::Command::new("ifconfig")
        .args([iface, "inet6", addr, "prefixlen", prefixlen, "alias"])
        .status()
        .map_err(|e| anyhow::anyhow!("ifconfig inet6 failed: {}", e))?;
    if !status.success() {
        anyhow::bail!("Failed to assign IPv6 {} to {}", cidr, iface);
    }
    tracing::info!("bridge {} inet6 {}", iface, cidr);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // build_vxlan_payload
    // -----------------------------------------------------------------------

    #[test]
    fn vxlan_header_ibit_set() {
        assert_eq!(build_vxlan_payload(&[], 0)[0], 0x08);
    }

    #[test]
    fn vxlan_header_reserved_bytes_zero() {
        let pkt = build_vxlan_payload(&[], 0);
        assert_eq!(&pkt[1..4], &[0x00; 3]);
        assert_eq!(pkt[7], 0x00);
    }

    #[test]
    fn vxlan_header_total_length() {
        let pkt = build_vxlan_payload(&[0xAAu8; 60], 1);
        assert_eq!(pkt.len(), VXLAN_HEADER_SIZE + 60);
    }

    #[test]
    fn vxlan_vni_encoding_typical() {
        let pkt = build_vxlan_payload(&[], 0x00_10_20_30);
        assert_eq!(&pkt[4..7], &[0x10, 0x20, 0x30]);
    }

    #[test]
    fn vxlan_vni_zero() {
        assert_eq!(&build_vxlan_payload(&[], 0)[4..7], &[0x00; 3]);
    }

    #[test]
    fn vxlan_vni_max() {
        assert_eq!(&build_vxlan_payload(&[], 0xFF_FF_FF)[4..7], &[0xFF; 3]);
    }

    #[test]
    fn vxlan_payload_appended() {
        let eth = [0x01u8, 0x02, 0x03, 0x04];
        let pkt = build_vxlan_payload(&eth, 42);
        assert_eq!(&pkt[VXLAN_HEADER_SIZE..], eth.as_ref());
    }

    #[test]
    fn vxlan_empty_eth_frame() {
        assert_eq!(build_vxlan_payload(&[], 1).len(), VXLAN_HEADER_SIZE);
    }

    // -----------------------------------------------------------------------
    // unwrap_vxlan
    // -----------------------------------------------------------------------

    fn make_frame(vni: u32, payload: &[u8]) -> Vec<u8> {
        build_vxlan_payload(payload, vni)
    }

    #[test]
    fn unwrap_valid_returns_inner_frame() {
        let eth = [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(unwrap_vxlan(&make_frame(100, &eth), 100).unwrap(), eth.as_ref());
    }

    #[test]
    fn unwrap_minimum_size_returns_empty_slice() {
        assert!(unwrap_vxlan(&make_frame(1, &[]), 1).unwrap().is_empty());
    }

    #[test]
    fn unwrap_too_short_returns_none() {
        assert!(unwrap_vxlan(&vec![0u8; VXLAN_HEADER_SIZE - 1], 0).is_none());
    }

    #[test]
    fn unwrap_empty_returns_none() {
        assert!(unwrap_vxlan(&[], 0).is_none());
    }

    #[test]
    fn unwrap_vni_mismatch_returns_none() {
        assert!(unwrap_vxlan(&make_frame(10, &[0xAA; 14]), 99).is_none());
    }

    #[test]
    fn unwrap_vni_zero_matches() {
        assert!(unwrap_vxlan(&make_frame(0, &[0x11; 14]), 0).is_some());
    }

    #[test]
    fn unwrap_vni_max_matches() {
        assert!(unwrap_vxlan(&make_frame(0xFF_FF_FF, &[0x22; 14]), 0xFF_FF_FF).is_some());
    }

    // -----------------------------------------------------------------------
    // Round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_preserves_payload() {
        let eth: Vec<u8> = (0u8..=63).collect();
        assert_eq!(unwrap_vxlan(&make_frame(0xABCDEF, &eth), 0xABCDEF).unwrap(), eth.as_slice());
    }

    #[test]
    fn roundtrip_wrong_vni_returns_none() {
        assert!(unwrap_vxlan(&make_frame(1, &[0xFF; 14]), 2).is_none());
    }

    // -----------------------------------------------------------------------
    // vxlan_to_vmnet — early-return paths (null ctx never reached)
    // -----------------------------------------------------------------------

    #[test]
    fn vxlan_to_vmnet_short_frame_returns_early() {
        // unwrap_vxlan returns None → function returns before touching ctx
        vxlan_to_vmnet(&[0u8; 4], 1, std::ptr::null_mut(), "bridge0");
    }

    #[test]
    fn vxlan_to_vmnet_vni_mismatch_returns_early() {
        let frame = make_frame(10, &[0xAA; 14]);
        vxlan_to_vmnet(&frame, 99, std::ptr::null_mut(), "bridge0");
    }

    // -----------------------------------------------------------------------
    // bridge naming
    // -----------------------------------------------------------------------

    #[test]
    fn bridge_name_matches_vni() {
        assert_eq!(bridge_name_for_vni(199).unwrap(), "bridge199");
    }

    #[test]
    fn bridge_name_accepts_flannel_vni() {
        assert_eq!(bridge_name_for_vni(1).unwrap(), "bridge1");
    }

    #[test]
    fn bridge_name_accepts_maximum_vni() {
        assert_eq!(bridge_name_for_vni(VXLAN_MAX_VNI).unwrap(), "bridge16777215");
    }

    #[test]
    fn bridge_name_rejects_vni_above_vxlan_range() {
        assert!(bridge_name_for_vni(VXLAN_MAX_VNI + 1).is_err());
    }

    // -----------------------------------------------------------------------
    // parse_bridge_names
    // -----------------------------------------------------------------------

    #[test]
    fn parse_bridges_empty_input() {
        assert!(parse_bridge_names("").is_empty());
    }

    #[test]
    fn parse_bridges_no_bridge_interfaces() {
        let input = "lo0: flags=8049\n\tinet 127.0.0.1\nen0: flags=8863\n";
        assert!(parse_bridge_names(input).is_empty());
    }

    #[test]
    fn parse_bridges_single() {
        let input = "en0: flags=8863\nbridge100: flags=8863\n\tinet 192.168.64.1\n";
        assert_eq!(parse_bridge_names(input), vec!["bridge100"]);
    }

    #[test]
    fn parse_bridges_multiple() {
        let input = "bridge0: flags=1\nbridge1: flags=2\nutun0: flags=3\nbridge100: flags=4\n";
        assert_eq!(parse_bridge_names(input), vec!["bridge0", "bridge1", "bridge100"]);
    }

    #[test]
    fn parse_bridges_ignores_indented_detail_lines() {
        let input = "bridge100: flags=8863\n\tether aa:bb:cc:dd:ee:ff\n\tinet 10.0.0.1\n";
        assert_eq!(parse_bridge_names(input), vec!["bridge100"]);
    }

    // -----------------------------------------------------------------------
    // parse_inet4_addrs
    // -----------------------------------------------------------------------

    #[test]
    fn parse_inet4_empty_input() {
        assert!(parse_inet4_addrs("").is_empty());
    }

    #[test]
    fn parse_inet4_no_addresses() {
        assert!(parse_inet4_addrs("bridge100: flags=8863\n\tether aa:bb:cc:dd:ee:ff\n").is_empty());
    }

    #[test]
    fn parse_inet4_single_address() {
        let input = "\tinet 192.168.64.1 netmask 0xffffff00 broadcast 192.168.64.255\n";
        assert_eq!(parse_inet4_addrs(input), vec!["192.168.64.1"]);
    }

    #[test]
    fn parse_inet4_multiple_addresses() {
        let input = "\tinet 10.0.0.1 netmask 0xff000000\n\tinet 172.16.0.1 netmask 0xffff0000\n";
        assert_eq!(parse_inet4_addrs(input), vec!["10.0.0.1", "172.16.0.1"]);
    }

    #[test]
    fn parse_inet4_ignores_inet6_lines() {
        let input = "\tinet6 fe80::1%bridge100 prefixlen 64\n\tinet 192.168.1.1 netmask 0xffffff00\n";
        assert_eq!(parse_inet4_addrs(input), vec!["192.168.1.1"]);
    }

    // -----------------------------------------------------------------------
    // find_new_bridge_in
    // -----------------------------------------------------------------------

    #[test]
    fn find_new_bridge_in_empty_current_returns_none() {
        assert!(find_new_bridge_in(&[], &[]).is_none());
    }

    #[test]
    fn find_new_bridge_in_all_known_returns_none() {
        let before = vec!["bridge0".to_string(), "bridge1".to_string()];
        assert!(find_new_bridge_in(&before, &before).is_none());
    }

    #[test]
    fn find_new_bridge_in_finds_new() {
        let before  = vec!["bridge0".to_string()];
        let current = vec!["bridge0".to_string(), "bridge100".to_string()];
        assert_eq!(find_new_bridge_in(&current, &before).unwrap(), "bridge100");
    }

    #[test]
    fn find_new_bridge_in_returns_first_new() {
        let before  = vec![];
        let current = vec!["bridge0".to_string(), "bridge1".to_string()];
        assert_eq!(find_new_bridge_in(&current, &before).unwrap(), "bridge0");
    }

    // -----------------------------------------------------------------------
    // parse_cidr
    // -----------------------------------------------------------------------

    #[test]
    fn parse_cidr_valid() {
        let (addr, prefix) = parse_cidr("fd00::1/64").unwrap();
        assert_eq!(addr, "fd00::1");
        assert_eq!(prefix, "64");
    }

    #[test]
    fn parse_cidr_ipv4_notation() {
        let (addr, prefix) = parse_cidr("192.168.1.1/24").unwrap();
        assert_eq!(addr, "192.168.1.1");
        assert_eq!(prefix, "24");
    }

    #[test]
    fn parse_cidr_missing_slash_returns_err() {
        assert!(parse_cidr("fd00::1").is_err());
    }

    // -----------------------------------------------------------------------
    // Bridge helpers — process-level (no root required for error paths)
    // -----------------------------------------------------------------------

    #[test]
    fn list_bridge_interfaces_does_not_panic() {
        let _ = list_bridge_interfaces(); // smoke: ifconfig is available, returns Vec
    }

    #[test]
    fn find_new_bridge_returns_none_for_stable_list() {
        // Snapshot current bridges; finding a "new" one against itself → None.
        let current = list_bridge_interfaces();
        assert!(find_new_bridge(&current).is_none());
    }

    #[test]
    fn remove_all_inet4_nonexistent_iface_is_noop() {
        // ifconfig on a nonexistent interface fails silently; no panic expected.
        remove_all_inet4("darwin_vxlan_test_nonexistent");
    }

    #[test]
    fn remove_inet4_addrs_executes_delete_for_each_address() {
        // Directly exercise the loop body with synthetic ifconfig output.
        // ifconfig exits non-zero (nonexistent interface) but the result is
        // ignored, so no panic is expected and the loop body is covered.
        let fake_output = "\tinet 10.0.0.1 netmask 0xffffff00 broadcast 10.0.0.255\n\
                           \tinet 10.0.0.2 netmask 0xffffff00\n";
        remove_inet4_addrs("darwin_vxlan_test_nonexistent", fake_output);
    }

    #[test]
    fn assign_inet4_fails_for_nonexistent_iface() {
        // ifconfig exits non-zero → our function returns Err (no root required
        // for the "interface does not exist" check on macOS).
        let result = assign_inet4("darwin_vxlan_test_nonexistent", "192.168.200.1/24");
        assert!(result.is_err());
    }

    #[test]
    fn assign_inet6_fails_for_nonexistent_iface() {
        let result = assign_inet6("darwin_vxlan_test_nonexistent", "fd00::1/64");
        assert!(result.is_err());
    }

    #[test]
    fn assign_inet6_fails_for_missing_slash() {
        // parse_cidr rejects the CIDR before ifconfig is ever called.
        let result = assign_inet6("bridge0", "fd00::1");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // VxlanTunnel — requires vmnet-mock feature
    // -----------------------------------------------------------------------

    #[cfg(feature = "vmnet-mock")]
    mod mock_tests {
        use super::*;

        fn local()  -> std::net::IpAddr { "127.0.0.1".parse().unwrap() }
        fn remote() -> std::net::IpAddr { "127.0.0.1".parse().unwrap() }

        /// Create a raw mock context for calling C-layer functions directly.
        /// Caller must pass it to `free_mock_ctx` when done.
        unsafe fn make_mock_ctx() -> *mut libc::c_void {
            unsafe { vmnet_ctx_start(1500) }
        }

        unsafe fn free_mock_ctx(ctx: *mut libc::c_void) {
            unsafe { vmnet_ctx_stop(ctx); }
        }

        // -------------------------------------------------------------------
        // Constructor paths
        // -------------------------------------------------------------------

        #[tokio::test]
        async fn tunnel_new_succeeds() {
            let t = VxlanTunnel::new(100, local(), remote(), 0, 1450, None, None)
                .await
                .expect("new() should succeed with mock backend");
            assert_eq!(t.vni, 100);
            assert_eq!(t.max_packet_size, 1450 + 18);
            assert_eq!(t.bridge_name, "mock_bridge0");
        }

        #[tokio::test]
        async fn tunnel_new_binds_explicit_flannel_port() {
            let t = VxlanTunnel::new(1, local(), remote(), 8472, 1450, None, None)
                .await
                .expect("new() should bind the requested port with mock backend");
            assert_eq!(t.local_addr().unwrap().port(), 8472);
            assert_eq!(t.remote_addrs, vec![SocketAddr::new(remote(), 8472)]);
        }

        #[tokio::test]
        async fn tunnel_new_with_remotes_rejects_empty_peer_list() {
            let result = VxlanTunnel::new_with_remotes(
                1,
                SocketAddr::new(local(), 0),
                Vec::new(),
                1450,
                None,
                None,
            ).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn tunnel_new_fails_when_ctx_null() {
            // mock_start_fail is _Thread_local in C, so this only affects the
            // current OS thread — other parallel tests are unaffected.
            unsafe { vmnet_mock_set_start_fail(1); }
            let result = VxlanTunnel::new(1, local(), remote(), 0, 1450, None, None).await;
            unsafe { vmnet_mock_set_start_fail(0); }
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn tunnel_new_propagates_ipv4_assignment_error() {
            // assign_inet4 will fail (nonexistent bridge) → new() returns Err.
            let result = VxlanTunnel::new(1, local(), remote(), 0, 1450, Some("10.0.0.1/24"), None).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn tunnel_new_propagates_ipv6_assignment_error() {
            // bridge_ipv6 is Some → assign_inet6 is called; mock_bridge0 does
            // not exist, so ifconfig fails → new() returns Err.
            let result = VxlanTunnel::new(1, local(), remote(), 0, 1450, None, Some("fd00::1/64")).await;
            assert!(result.is_err());
        }

        // -------------------------------------------------------------------
        // Drop
        // -------------------------------------------------------------------

        #[tokio::test]
        async fn tunnel_drop_calls_stop() {
            // Drop is implicit when `t` goes out of scope; vmnet_ctx_stop closes
            // the pipes. Verify no double-free / use-after-free by constructing
            // and immediately dropping the tunnel.
            let t = VxlanTunnel::new(1, local(), remote(), 0, 1450, None, None)
                .await
                .unwrap();
            drop(t); // explicit to make the intent clear
        }

        // -------------------------------------------------------------------
        // vxlan_to_vmnet — write path (requires a live mock ctx)
        // -------------------------------------------------------------------

        #[test]
        fn vxlan_to_vmnet_valid_frame_calls_write() {
            // unwrap_vxlan returns Some → vmnet_ctx_write is called (lines 240-241).
            let ctx = unsafe { make_mock_ctx() };
            let frame = make_frame(42, &[0xABu8; 14]);
            vxlan_to_vmnet(&frame, 42, ctx, "mock_bridge0");
            unsafe { free_mock_ctx(ctx); }
        }

        #[test]
        fn vxlan_to_vmnet_write_fail_logs_warning() {
            // vmnet_ctx_write returns -1 → the warning branch executes (lines 242-243).
            let ctx = unsafe { make_mock_ctx() };
            unsafe { vmnet_mock_set_write_fail(1); }
            let frame = make_frame(99, &[0xCDu8; 14]);
            vxlan_to_vmnet(&frame, 99, ctx, "mock_bridge0");
            unsafe {
                vmnet_mock_set_write_fail(0);
                free_mock_ctx(ctx);
            }
        }

        // -------------------------------------------------------------------
        // run() lifecycle
        // -------------------------------------------------------------------

        /// Exercise the full run_until() lifecycle: tasks are spawned, the
        /// shutdown future resolves, the blocking thread is unblocked, and
        /// run_until() returns Ok.
        ///
        /// The mock pre-fills notify_pipe so the blocking-thread read path and
        /// the forwarding-task path are exercised before shutdown.
        #[tokio::test]
        async fn tunnel_run_until_starts_and_shuts_down() {
            let tunnel = VxlanTunnel::new(1, local(), remote(), 0, 1450, None, None)
                .await
                .unwrap();

            let result = tunnel.run_until(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(())
            }).await;
            assert!(result.is_ok(), "run_until() should return Ok: {:?}", result.err());
        }

        /// A shutdown error still cancels and joins the forwarding workers
        /// before the tunnel is dropped.
        #[tokio::test]
        async fn tunnel_run_until_cleans_up_when_shutdown_fails() {
            let tunnel = VxlanTunnel::new(1, local(), remote(), 0, 1450, None, None)
                .await
                .unwrap();

            let result = tunnel.run_until(async {
                Err(std::io::Error::other("test shutdown failure"))
            }).await;
            assert!(result.is_err());
        }

        /// Verify that `run()` delegates to `run_until(ctrl_c())`.
        /// This is the only test in the suite that sends SIGINT; all other
        /// tests use `run_until` with a timer, so there is no interference.
        #[tokio::test]
        async fn tunnel_run_delegates_to_ctrl_c() {
            let tunnel = VxlanTunnel::new(1, local(), remote(), 0, 1450, None, None)
                .await
                .unwrap();
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                unsafe { libc::kill(libc::getpid(), libc::SIGINT); }
            });
            let result = tunnel.run().await;
            assert!(result.is_ok(), "run() should return Ok: {:?}", result.err());
        }

        /// Verify that one Ethernet frame is sent to every configured VTEP
        /// while all peers share the tunnel's single UDP socket.
        #[tokio::test]
        async fn tunnel_run_until_fans_out_to_all_remote_peers() {
            let peer_a = tokio::net::UdpSocket::bind(SocketAddr::new(local(), 0)).await.unwrap();
            let peer_b = tokio::net::UdpSocket::bind(SocketAddr::new(local(), 0)).await.unwrap();
            let peer_a_addr = peer_a.local_addr().unwrap();
            let peer_b_addr = peer_b.local_addr().unwrap();
            let tunnel = VxlanTunnel::new_with_remotes(
                77,
                SocketAddr::new(local(), 0),
                vec![peer_a_addr, peer_b_addr],
                1450,
                None,
                None,
            ).await.unwrap();

            let expected = build_vxlan_payload(&[0xAAu8; 14], 77);
            let (packet_a, packet_b, result) = tokio::join!(
                async move {
                    let mut buf = vec![0u8; 65535];
                    tokio::time::timeout(Duration::from_secs(1), peer_a.recv_from(&mut buf))
                        .await
                        .expect("peer A should receive a fan-out packet")
                        .map(|(len, _)| buf[..len].to_vec())
                },
                async move {
                    let mut buf = vec![0u8; 65535];
                    tokio::time::timeout(Duration::from_secs(1), peer_b.recv_from(&mut buf))
                        .await
                        .expect("peer B should receive a fan-out packet")
                        .map(|(len, _)| buf[..len].to_vec())
                },
                tunnel.run_until(async {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(())
                }),
            );

            assert!(result.is_ok(), "run_until() failed: {:?}", result.err());
            assert_eq!(packet_a.unwrap(), expected);
            assert_eq!(packet_b.unwrap(), expected);
        }

        /// UDP self-loopback: bind the tunnel to port 0, discover the assigned
        /// port via local_addr(), send a valid VXLAN datagram from a second
        /// socket, then shut down via run_until.  The recv task processes the
        /// datagram (UDP recv Ok path) and calls vxlan_to_vmnet.
        #[tokio::test]
        async fn tunnel_run_until_udp_recv_task_processes_packet() {
            let vni = 77u32;
            let tunnel = VxlanTunnel::new(vni, local(), local(), 0, 1450, None, None)
                .await
                .unwrap();

            let bound_addr = tunnel.local_addr().expect("local_addr");

            // Send a VXLAN packet to the tunnel's own socket a short time after
            // run_until() starts, giving the recv task time to reach recv_from().
            let vxlan_pkt = build_vxlan_payload(&[0xEFu8; 14], vni);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                s.send_to(&vxlan_pkt, bound_addr).await.ok();
            });

            let result = tunnel.run_until(async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(())
            }).await;
            assert!(result.is_ok(), "run_until() should return Ok: {:?}", result.err());
        }
    }
}
