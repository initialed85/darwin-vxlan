mod vxlan;

use anyhow::Result;
use clap::Parser;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_VXLAN_PORT: u16 = 4789;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PeerSpec {
    underlay: IpAddr,
    pod_cidr: Option<vxlan::IpCidr>,
    vtep_mac: Option<[u8; 6]>,
}

impl FromStr for PeerSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some((pod_cidr, underlay)) = value.split_once('=') {
            let pod_cidr = pod_cidr
                .parse()
                .map_err(|error| format!("invalid peer CIDR {pod_cidr}: {error}"))?;
            let underlay = underlay
                .parse()
                .map_err(|_| format!("invalid underlay IP address: {underlay}"))?;
            return Ok(Self {
                underlay,
                pod_cidr: Some(pod_cidr),
                vtep_mac: None,
            });
        }

        // Keep the earlier API shape available to direct callers while the
        // CLI's documented form uses PodCIDR=underlay.
        let (underlay, mac) = value
            .split_once(',')
            .ok_or_else(|| "expected POD_CIDR=UNDERLAY_IP".to_string())?;
        let underlay = underlay
            .parse()
            .map_err(|_| format!("invalid underlay IP address: {underlay}"))?;
        let octets: Vec<&str> = mac.split(':').collect();
        if octets.len() != 6 || octets.iter().any(|octet| octet.len() != 2) {
            return Err(format!("invalid VTEP MAC address: {mac}"));
        }
        let mut vtep_mac = [0u8; 6];
        for (index, octet) in octets.iter().enumerate() {
            vtep_mac[index] = u8::from_str_radix(octet, 16)
                .map_err(|_| format!("invalid VTEP MAC address: {mac}"))?;
        }
        Ok(Self {
            underlay,
            pod_cidr: None,
            vtep_mac: Some(vtep_mac),
        })
    }
}

#[derive(Parser, Debug)]
#[command(name = "darwin-vxlan")]
#[command(about = "Userspace L2 VXLAN tunnel for macOS (Apple Silicon)", long_about = None)]
struct Args {
    #[arg(long, help = "VXLAN Network Identifier (VNI)")]
    vni: u32,

    #[arg(long, help = "Local IP address for the VXLAN UDP socket")]
    local: IpAddr,

    #[arg(
        long = "remote",
        value_name = "IP",
        required_unless_present = "peer_specs",
        action = clap::ArgAction::Append,
        value_delimiter = ',',
        help = "Unmapped remote VTEP IP fallback (repeat for each peer)",
    )]
    remotes: Vec<IpAddr>,

    #[arg(
        long = "peer",
        value_name = "POD_CIDR=UNDERLAY_IP",
        required_unless_present = "remotes",
        action = clap::ArgAction::Append,
        help = "Map an inner destination PodCIDR to an underlay IP (repeat per peer)",
    )]
    peer_specs: Vec<PeerSpec>,

    #[arg(
        long,
        default_value_t = DEFAULT_VXLAN_PORT,
        value_name = "PORT",
        help = "UDP port for the local VXLAN listen/bind and every remote destination (use 8472 for K3s Flannel)",
    )]
    port: u16,

    #[arg(long, default_value = "1450", help = "MTU for the bridge interface")]
    mtu: u32,

    #[arg(long = "bridge-ipv4", value_name = "CIDR", help = "IPv4 address to assign to the bridge (e.g. 192.168.100.1/24)")]
    bridge_ipv4: Option<String>,

    #[arg(long = "bridge-ipv6", value_name = "CIDR", help = "IPv6 address to assign to the bridge (e.g. fd00::1/64)")]
    bridge_ipv6: Option<String>,
}

async fn run(args: Args) -> Result<()> {
    run_until(args, tokio::signal::ctrl_c()).await
}

async fn run_until<F>(args: Args, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tracing::info!(
        "Starting darwin-vxlan — VNI {} | local {} | remotes {:?} | peers {:?} | port {}",
        args.vni, args.local, args.remotes, args.peer_specs, args.port
    );

    let local_addr = SocketAddr::new(args.local, args.port);
    let peers = args.peer_specs.iter()
        .map(|peer| vxlan::VtepPeer {
            endpoint: SocketAddr::new(peer.underlay, args.port),
            pod_cidr: peer.pod_cidr.clone(),
            vtep_mac: peer.vtep_mac,
        })
        .chain(args.remotes.iter().map(|remote| vxlan::VtepPeer {
            endpoint: SocketAddr::new(*remote, args.port),
            pod_cidr: None,
            vtep_mac: None,
        }))
        .collect();
    let tunnel = if args.peer_specs.is_empty() && args.remotes.len() == 1 {
        // Keep the original point-to-point constructor on the common path.
        vxlan::VxlanTunnel::new(
            args.vni,
            args.local,
            args.remotes[0],
            args.port,
            args.mtu,
            args.bridge_ipv4.as_deref(),
            args.bridge_ipv6.as_deref(),
        ).await?
    } else {
        vxlan::VxlanTunnel::new_with_peers(
            args.vni,
            local_addr,
            peers,
            args.mtu,
            args.bridge_ipv4.as_deref(),
            args.bridge_ipv6.as_deref(),
        ).await?
    };

    tunnel.run_until(shutdown).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt::Subscriber::builder()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init()
        .ok();

    let args = Args::parse();
    run(args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    

    #[test]
    fn args_parse_required_fields_only() {
        let a = Args::try_parse_from([
            "darwin-vxlan", "--vni", "100",
            "--local", "0.0.0.0", "--remote", "1.2.3.4",
        ]).unwrap();
        assert_eq!(a.vni, 100);
        assert_eq!(a.remotes, vec!["1.2.3.4".parse::<IpAddr>().unwrap()]);
        assert!(a.peer_specs.is_empty());
        assert_eq!(a.port, DEFAULT_VXLAN_PORT);  // default
        assert_eq!(a.mtu, 1450);   // default
        assert!(a.bridge_ipv4.is_none());
        assert!(a.bridge_ipv6.is_none());
    }

    #[test]
    fn args_parse_flannel_port() {
        let a = Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "192.168.1.10", "--remote", "192.168.1.11",
            "--port", "8472",
        ]).unwrap();
        assert_eq!(a.vni, 1);
        assert_eq!(a.remotes, vec!["192.168.1.11".parse::<IpAddr>().unwrap()]);
        assert_eq!(a.port, 8472);
    }

    #[test]
    fn args_parse_multiple_remote_peers() {
        let a = Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "192.168.1.10",
            "--remote", "192.168.1.11",
            "--remote", "192.168.1.12",
            "--port", "8472",
        ]).unwrap();
        assert_eq!(a.remotes, vec![
            "192.168.1.11".parse::<IpAddr>().unwrap(),
            "192.168.1.12".parse::<IpAddr>().unwrap(),
        ]);
        assert_eq!(a.port, 8472);
    }

    #[test]
    fn args_parse_comma_delimited_remote_peers() {
        let a = Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "192.168.1.10",
            "--remote", "192.168.1.11,192.168.1.12",
        ]).unwrap();
        assert_eq!(a.remotes, vec![
            "192.168.1.11".parse::<IpAddr>().unwrap(),
            "192.168.1.12".parse::<IpAddr>().unwrap(),
        ]);
    }

    #[test]
    fn args_parse_peer_mappings() {
        let a = Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "192.168.1.10",
            "--peer", "10.42.1.0/24=192.168.1.111",
            "--peer", "10.42.2.0/24=192.168.1.112",
            "--port", "8472",
        ]).unwrap();
        assert!(a.remotes.is_empty());
        assert_eq!(
            a.peer_specs,
            vec![
                PeerSpec {
                    underlay: "192.168.1.111".parse().unwrap(),
                    pod_cidr: Some("10.42.1.0/24".parse().unwrap()),
                    vtep_mac: None,
                },
                PeerSpec {
                    underlay: "192.168.1.112".parse().unwrap(),
                    pod_cidr: Some("10.42.2.0/24".parse().unwrap()),
                    vtep_mac: None,
                },
            ]
        );
        assert_eq!(a.port, 8472);
    }

    #[test]
    fn args_invalid_peer_mapping_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "192.168.1.10",
            "--peer", "not-a-cidr=192.168.1.111",
        ]).is_err());
    }

    #[test]
    fn args_parse_all_fields() {
        let a = Args::try_parse_from([
            "darwin-vxlan", "--vni", "42",
            "--local", "10.0.0.1", "--remote", "10.0.0.2",
            "--port", "9999", "--mtu", "1400",
            "--bridge-ipv4", "192.168.100.1/24",
            "--bridge-ipv6", "fd00::1/64",
        ]).unwrap();
        assert_eq!(a.vni, 42);
        assert_eq!(a.remotes, vec!["10.0.0.2".parse::<IpAddr>().unwrap()]);
        assert!(a.peer_specs.is_empty());
        assert_eq!(a.port, 9999);
        assert_eq!(a.mtu, 1400);
        assert_eq!(a.bridge_ipv4.as_deref(), Some("192.168.100.1/24"));
        assert_eq!(a.bridge_ipv6.as_deref(), Some("fd00::1/64"));
    }

    #[test]
    fn args_missing_remote_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--vni", "1", "--local", "0.0.0.0",
        ]).is_err());
    }

    #[test]
    fn args_missing_vni_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--local", "0.0.0.0", "--remote", "1.2.3.4",
        ]).is_err());
    }

    #[test]
    fn args_invalid_ip_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "not-an-ip", "--remote", "1.2.3.4",
        ]).is_err());
    }

    #[test]
    fn args_missing_local_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--vni", "1", "--remote", "1.2.3.4",
        ]).is_err());
    }

    #[test]
    fn args_invalid_port_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--vni", "1",
            "--local", "0.0.0.0", "--remote", "1.2.3.4",
            "--port", "not-a-port",
        ]).is_err());
    }

    #[test]
    fn args_invalid_vni_fails() {
        assert!(Args::try_parse_from([
            "darwin-vxlan", "--vni", "not-a-number",
            "--local", "0.0.0.0", "--remote", "1.2.3.4",
        ]).is_err());
    }

    /// Tests for `run_until()` that require the vmnet-mock backend.
    #[cfg(feature = "vmnet-mock")]
    mod mock_tests {
        use super::*;
        use std::time::Duration;

        fn base_args() -> Args {
            Args::try_parse_from([
                "darwin-vxlan", "--vni", "1",
                "--local", "127.0.0.1", "--remote", "127.0.0.1",
                "--port", "0",
            ]).unwrap()
        }

        /// Verify that `run()` delegates to `run_until(ctrl_c())`.
        /// This is the only test in main.rs that sends SIGINT; all other tests
        /// use `run_until()` with a timer, so there is no interference.
        #[tokio::test]
        async fn run_delegates_to_ctrl_c() {
            let args = base_args();
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                unsafe { libc::kill(libc::getpid(), libc::SIGINT); }
            });
            let result = run(args).await;
            assert!(result.is_ok(), "run() should return Ok: {:?}", result.err());
        }

        /// run_until() propagates an Err from tunnel creation (bad bridge IPv4).
        /// Covers the tracing::info! and VxlanTunnel::new() call site.
        #[tokio::test]
        async fn run_until_propagates_tunnel_creation_error() {
            let args = Args::try_parse_from([
                "darwin-vxlan", "--vni", "1",
                "--local", "127.0.0.1", "--remote", "127.0.0.1",
                "--port", "0", "--bridge-ipv4", "10.0.0.1/24",
            ]).unwrap();
            let result = run_until(args, async { Ok(()) }).await;
            assert!(result.is_err());
        }

        /// run_until() creates a tunnel, runs it, and shuts down cleanly.
        /// Covers tunnel.run_until() and Ok(()) inside run_until().
        #[tokio::test]
        async fn run_until_with_mock_shuts_down_cleanly() {
            let result = run_until(base_args(), async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(())
            }).await;
            assert!(result.is_ok(), "run_until() failed: {:?}", result.err());
        }
    }
}
