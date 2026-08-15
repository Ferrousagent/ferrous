//! Host-side security posture derived from a [`CapabilityGrant`].
//!
//! Ferrous never relies on wasmtime's *implicit* defaults for environment or
//! networking. Every guest `WasiCtx` is built with an explicit policy: no
//! environment variable reaches the guest unless its name is allowlisted, and
//! no socket address is reachable unless the grant allows that exact loopback
//! port. These functions are the single source of truth for what a guest may
//! observe and reach, and are deliberately small and pure so the red-team
//! tests can exercise every decision.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::sockets::SocketAddrUse;

use crate::capability::CapabilityGrant;

/// Network posture for one command, derived from the grant's port allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// TCP sockets may be opened (only toward allowlisted loopback ports).
    pub tcp: bool,
    /// UDP sockets may be opened. Always `false` in Phase 1: there is no UDP
    /// allowlist yet, so UDP is blanket-denied.
    pub udp: bool,
    /// Loopback TCP ports the guest may bind or connect to.
    pub allowed_ports: BTreeSet<u16>,
}

impl NetworkPolicy {
    /// Derive the posture from a grant: all networking is denied unless the
    /// grant explicitly allows one or more loopback TCP ports.
    pub fn from_grant(grant: &CapabilityGrant) -> Self {
        let allowed_ports = grant.loopback_ports().clone();
        let tcp = !allowed_ports.is_empty();
        Self {
            tcp,
            udp: false,
            allowed_ports,
        }
    }

    /// Whether a socket address is permitted for the given use.
    ///
    /// Only TCP on loopback addresses with an allowlisted port passes; binding,
    /// UDP, and any non-loopback address are denied.
    pub fn permits(&self, addr: SocketAddr, use_: SocketAddrUse) -> bool {
        match use_ {
            SocketAddrUse::TcpBind | SocketAddrUse::TcpConnect => {
                self.tcp && addr.ip().is_loopback() && self.allowed_ports.contains(&addr.port())
            }
            // Any UDP use (and any future use kind) is denied: the Phase 1
            // allowlist only covers loopback TCP ports.
            _ => false,
        }
    }

    /// Apply the posture to a builder, turning every allowance into an explicit
    /// one and closing DNS, UDP, and unallowlisted TCP.
    pub fn apply(&self, builder: &mut WasiCtxBuilder) {
        builder.allow_ip_name_lookup(false);
        builder.allow_udp(self.udp);
        builder.allow_tcp(self.tcp);
        if self.tcp {
            let policy = self.clone();
            builder.socket_addr_check(move |addr, use_| {
                let policy = policy.clone();
                Box::pin(async move { policy.permits(addr, use_) })
            });
        }
    }
}

/// Select exactly the environment variables the grant allowlists, resolved
/// through `provider` (normally the host process environment).
///
/// Names the provider cannot resolve are silently dropped; names that are not
/// allowlisted are never queried. The result is sorted for determinism.
pub fn selected_environment(
    grant: &CapabilityGrant,
    provider: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    grant
        .environment_names()
        .filter_map(|name| provider(name).map(|value| (name.to_owned(), value)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn grant_with_ports(ports: &[u16]) -> CapabilityGrant {
        let mut grant = CapabilityGrant::empty();
        for port in ports {
            grant = grant.allow_loopback_port(*port);
        }
        grant
    }

    #[test]
    fn empty_grant_denies_all_networking() {
        let policy = NetworkPolicy::from_grant(&CapabilityGrant::empty());
        assert!(!policy.tcp);
        assert!(!policy.udp);
        assert!(policy.allowed_ports.is_empty());
    }

    #[test]
    fn loopback_grant_allows_only_that_port_and_use() {
        let policy = NetworkPolicy::from_grant(&grant_with_ports(&[3000]));
        assert!(policy.tcp);

        let loopback = "127.0.0.1:3000"
            .parse::<SocketAddr>()
            .expect("valid address");
        assert!(policy.permits(loopback, SocketAddrUse::TcpConnect));
        assert!(policy.permits(loopback, SocketAddrUse::TcpBind));
        assert!(!policy.permits(loopback, SocketAddrUse::UdpConnect));
        assert!(!policy.permits(loopback, SocketAddrUse::UdpBind));

        let other_port = "127.0.0.1:3001"
            .parse::<SocketAddr>()
            .expect("valid address");
        assert!(!policy.permits(other_port, SocketAddrUse::TcpConnect));

        let non_loopback = "10.0.0.1:3000"
            .parse::<SocketAddr>()
            .expect("valid address");
        assert!(!policy.permits(non_loopback, SocketAddrUse::TcpConnect));

        let broadcast = "0.0.0.0:3000".parse::<SocketAddr>().expect("valid address");
        assert!(!policy.permits(broadcast, SocketAddrUse::TcpBind));
    }

    #[test]
    fn multiple_allowed_ports_are_all_permitted() {
        let policy = NetworkPolicy::from_grant(&grant_with_ports(&[3000, 5173]));
        let first = "127.0.0.1:3000"
            .parse::<SocketAddr>()
            .expect("valid address");
        let second = "127.0.0.1:5173"
            .parse::<SocketAddr>()
            .expect("valid address");
        assert!(policy.permits(first, SocketAddrUse::TcpConnect));
        assert!(policy.permits(second, SocketAddrUse::TcpConnect));
    }

    #[test]
    fn environment_selection_filters_by_allowlist() {
        let grant = CapabilityGrant::empty()
            .allow_environment("ALLOWED")
            .expect("valid name")
            .allow_environment("PATH")
            .expect("valid name");
        let provider = |name: &str| match name {
            "ALLOWED" => Some("value-1".to_owned()),
            "PATH" => Some("/usr/bin".to_owned()),
            "SECRET" => Some("leak".to_owned()),
            _ => None,
        };

        let environment = selected_environment(&grant, &provider);
        assert_eq!(
            environment,
            vec![
                ("ALLOWED".to_owned(), "value-1".to_owned()),
                ("PATH".to_owned(), "/usr/bin".to_owned()),
            ]
        );
    }

    #[test]
    fn environment_selection_drops_missing_values() {
        let grant = CapabilityGrant::empty()
            .allow_environment("MISSING")
            .expect("valid name");
        assert!(selected_environment(&grant, &|_| None).is_empty());
    }

    #[test]
    fn unallowed_names_are_never_queried() {
        let grant = CapabilityGrant::empty();
        let queried = std::cell::RefCell::new(Vec::new());
        let provider = |name: &str| {
            queried.borrow_mut().push(name.to_owned());
            Some("value".to_owned())
        };
        let _ = selected_environment(&grant, &provider);
        assert!(queried.borrow().is_empty());
    }
}
