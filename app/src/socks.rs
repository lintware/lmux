//! A SOCKS5 relay for browser pages that run in this app but belong to a
//! remote host. WebKit sends every connection of such a page to this relay.
//! Names only the host can reach (its `localhost`, private addresses, names
//! with no public DNS) go to the SOCKS port of the host's ssh tunnel
//! (`ssh -D`); public sites connect straight from this Mac, since the link to
//! a host can be far slower than the Mac's own.
//!
//! WebKit never proxies loopback names (`localhost`, `127.0.0.1`,
//! `*.localhost`), so a page asks for [`HOST_LOOPBACK`] instead (see
//! [`to_page_url`]) and the relay turns that back into `localhost`.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The name a page uses for its host's own `localhost`.
pub const HOST_LOOPBACK: &str = "localhost.lmux";

/// A running relay. Its port stays the same across reconnects; only the
/// upstream ssh port changes ([`Relay::set_upstream`]).
pub struct Relay {
    port: u16,
    upstream: Arc<AtomicU16>,
    guarded: Arc<AtomicU16>,
}

impl Relay {
    /// Listen on a free loopback port. Connections fail until an upstream is set.
    /// `guarded` is the host daemon's port: the host's loopback is never
    /// reachable on it, since its WebSocket would let a page type into terminals.
    pub fn start(guarded: u16) -> io::Result<Relay> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let upstream = Arc::new(AtomicU16::new(0));
        let up = upstream.clone();
        let guarded = Arc::new(AtomicU16::new(guarded));
        let guard = guarded.clone();
        std::thread::Builder::new().name("lmux-socks".into()).spawn(move || {
            for conn in listener.incoming().flatten() {
                let up = up.load(Ordering::Relaxed);
                let guard = guard.load(Ordering::Relaxed);
                let _ = std::thread::Builder::new().name("lmux-socks-conn".into()).spawn(move || {
                    let _ = relay(conn, up, guard);
                });
            }
        })?;
        Ok(Relay { port, upstream, guarded })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The host daemon's port, which the host's loopback may not be reached on.
    pub fn set_guarded_port(&self, port: u16) {
        self.guarded.store(port, Ordering::Relaxed);
    }

    /// The SOCKS port of the host's ssh tunnel (0: none, connections fail).
    pub fn set_upstream(&self, port: u16) {
        self.upstream.store(port, Ordering::Relaxed);
    }
}

/// Where a SOCKS request wants to go.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Name(String),
    V4([u8; 4]),
    V6([u8; 16]),
}

/// Serve one client: its handshake, a connection through the upstream SOCKS
/// server, then bytes both ways until either side closes.
fn relay(mut client: TcpStream, upstream: u16, guarded: u16) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (target, port) = read_request(&mut client)?;
    let target = map_target(target);
    let up = match decide(&target, port, guarded) {
        Route::Refuse => Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused")),
        Route::Direct(s) => Ok(s),
        Route::Upstream if upstream == 0 => Err(io::Error::new(io::ErrorKind::NotConnected, "no tunnel")),
        Route::Upstream => connect_upstream(upstream, &target, port),
    };
    let mut up = match up {
        Ok(s) => s,
        Err(e) => {
            // General failure, bound to 0.0.0.0:0.
            let _ = client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
            return Err(e);
        }
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])?;
    client.set_read_timeout(None)?;
    up.set_read_timeout(None)?;
    let _ = client.set_nodelay(true);
    let _ = up.set_nodelay(true);
    let (mut c2, mut u2) = (client.try_clone()?, up.try_clone()?);
    let back = std::thread::spawn(move || {
        let _ = io::copy(&mut u2, &mut c2);
        let _ = c2.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut client, &mut up);
    let _ = up.shutdown(Shutdown::Write);
    let _ = back.join();
    Ok(())
}

/// Read a client's greeting (answering "no auth") and its CONNECT request.
fn read_request(s: &mut (impl Read + Write)) -> io::Result<(Target, u16)> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head)?;
    if head[0] != 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
    }
    let mut methods = vec![0u8; head[1] as usize];
    s.read_exact(&mut methods)?;
    s.write_all(&[5, 0])?;
    let mut req = [0u8; 4];
    s.read_exact(&mut req)?;
    if req[1] != 1 {
        // Only CONNECT: "command not supported".
        s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0])?;
        return Err(io::Error::new(io::ErrorKind::Unsupported, "only CONNECT"));
    }
    let target = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a)?;
            Target::V4(a)
        }
        3 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len)?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name)?;
            Target::Name(String::from_utf8_lossy(&name).into_owned())
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a)?;
            Target::V6(a)
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad address type")),
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port)?;
    Ok((target, u16::from_be_bytes(port)))
}

/// The page's stand-in name for its host's loopback becomes `localhost`.
fn map_target(t: Target) -> Target {
    match t {
        Target::Name(n) if n.eq_ignore_ascii_case(HOST_LOOPBACK) => Target::Name("localhost".into()),
        other => other,
    }
}

/// Addresses only the host can reach: loopback, private, link-local, CGNAT,
/// "this network", benchmarking, broadcast and multicast/reserved ranges.
fn private_v4(ip: Ipv4Addr) -> bool {
    let a = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || a[0] == 0
        || a[0] >= 224
        || a[0] == 100 && (a[1] & 0xC0) == 64
        || a[0] == 198 && (a[1] & 0xFE) == 18
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => private_v4(v),
        IpAddr::V6(v) => {
            // `::ffff:a.b.c.d` and the NAT64 prefix `64:ff9b::/96` carry an IPv4 address.
            if let Some(v4) = v.to_ipv4_mapped() {
                return private_v4(v4);
            }
            let o = v.octets();
            if o[..12] == [0, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                return private_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            }
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (o[0] & 0xFE) == 0xFC || (o[0] == 0xFE && (o[1] & 0xC0) == 0x80)
        }
    }
}

/// Targets that must go through the host: its loopback, private and
/// link-local addresses, and names that are not public DNS names.
fn host_only(t: &Target) -> bool {
    match t {
        Target::V4(a) => private_ip(IpAddr::V4(Ipv4Addr::from(*a))),
        Target::V6(a) => private_ip(IpAddr::V6(Ipv6Addr::from(*a))),
        Target::Name(n) => {
            let n = n.trim_end_matches('.').to_ascii_lowercase();
            !n.contains('.')
                || n == "localhost"
                || [".localhost", ".local", ".internal", ".lan", ".home.arpa", ".ts.net", ".corp"].iter().any(|s| n.ends_with(s))
                || n.parse::<IpAddr>().is_ok_and(private_ip)
        }
    }
}

/// The host's own loopback, however it is written.
fn loopback_target(t: &Target) -> bool {
    let lo = |ip: IpAddr| match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_unspecified(),
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(v.is_loopback() || v.is_unspecified(), |m| m.is_loopback() || m.is_unspecified()),
    };
    match t {
        Target::V4(a) => lo(IpAddr::V4(Ipv4Addr::from(*a))),
        Target::V6(a) => lo(IpAddr::V6(Ipv6Addr::from(*a))),
        Target::Name(n) => {
            let n = n.trim_end_matches('.').to_ascii_lowercase();
            n == "localhost" || n.ends_with(".localhost") || n.parse::<IpAddr>().is_ok_and(lo)
        }
    }
}

/// What to do with a request.
enum Route {
    Refuse,
    Direct(TcpStream),
    Upstream,
}

/// Where a request goes. Names that only the host can reach (by the name
/// rules) go to the host, except its loopback on the daemon's port. A public
/// name goes out from this Mac; if it resolves here to a loopback address it
/// is refused (DNS rebinding), and to a private one it goes to the host by
/// name. Only a failed resolution or a private answer goes up, never a failed
/// connect, and never to the daemon's port.
fn decide(t: &Target, port: u16, guarded: u16) -> Route {
    decide_with(t, port, guarded, &|n, p| {
        use std::net::ToSocketAddrs;
        (n, p).to_socket_addrs().map(|a| a.collect())
    })
}

fn decide_with(
    t: &Target,
    port: u16,
    guarded: u16,
    resolve: &dyn Fn(&str, u16) -> io::Result<Vec<std::net::SocketAddr>>,
) -> Route {
    if host_only(t) {
        // A single-label or odd name may be the host's own loopback.
        let may_be_loopback = loopback_target(t) || matches!(t, Target::Name(n) if !n.trim_end_matches('.').contains('.'));
        if guarded != 0 && port == guarded && may_be_loopback {
            return Route::Refuse;
        }
        return Route::Upstream;
    }
    use std::net::SocketAddr;
    // The host's DNS may answer a name with its own loopback (the Mac saw
    // NXDOMAIN or a private address), so no name goes up to the daemon's port.
    let upstream = || if guarded != 0 && port == guarded { Route::Refuse } else { Route::Upstream };
    let addrs: Vec<SocketAddr> = match t {
        Target::V4(a) => vec![SocketAddr::from((*a, port))],
        Target::V6(a) => vec![SocketAddr::from((*a, port))],
        Target::Name(n) => match resolve(n, port) {
            Ok(a) => a,
            // Not known here: the host's DNS may know it.
            Err(_) => return upstream(),
        },
    };
    let on_mac_loopback = |ip: IpAddr| match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_unspecified(),
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(v.is_loopback() || v.is_unspecified(), |m| m.is_loopback() || m.is_unspecified()),
    };
    // Loopback on the Mac is DNS rebinding; refuse it.
    if addrs.iter().any(|a| on_mac_loopback(a.ip())) {
        return Route::Refuse;
    }
    if addrs.iter().any(|a| private_ip(a.ip())) {
        // A private address the Mac sees (split DNS, VPN) names a machine the
        // host may reach by the same name; connecting from here would not.
        return match t {
            Target::Name(_) => upstream(),
            _ => Route::Refuse,
        };
    }
    match addrs.iter().find_map(|a| TcpStream::connect_timeout(a, Duration::from_secs(5)).ok()) {
        Some(s) => Route::Direct(s),
        // Resolved here but unreachable: the host is not asked.
        None => Route::Refuse,
    }
}

/// Open `target:port` through the SOCKS5 server at `127.0.0.1:upstream`.
fn connect_upstream(upstream: u16, target: &Target, port: u16) -> io::Result<TcpStream> {
    let mut s = TcpStream::connect(("127.0.0.1", upstream))?;
    s.set_read_timeout(Some(Duration::from_secs(20)))?;
    s.write_all(&[5, 1, 0])?;
    let mut reply = [0u8; 2];
    s.read_exact(&mut reply)?;
    if reply != [5, 0] {
        return Err(io::Error::other("upstream refused the handshake"));
    }
    let mut req = vec![5, 1, 0];
    match target {
        Target::V4(a) => {
            req.push(1);
            req.extend_from_slice(a);
        }
        Target::Name(n) => {
            req.push(3);
            req.push(n.len().min(255) as u8);
            req.extend_from_slice(&n.as_bytes()[..n.len().min(255)]);
        }
        Target::V6(a) => {
            req.push(4);
            req.extend_from_slice(a);
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head)?;
    if head[1] != 0 {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("upstream reply {}", head[1])));
    }
    // Skip the bound address.
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len)?;
            len[0] as usize
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad upstream reply")),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest)?;
    Ok(s)
}

/// Split `url` into (scheme+"://", authority host, rest-from-port-or-path).
fn split_host(url: &str) -> Option<(&str, &str, &str)> {
    let i = url.find("://")? + 3;
    let (pre, rest) = url.split_at(i);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    // Credentials are rare in a dev URL; leave such URLs alone.
    if authority.contains('@') {
        return None;
    }
    let host_end = if authority.starts_with('[') {
        authority.find(']').map(|j| j + 1)?
    } else {
        authority.find(':').unwrap_or(authority.len())
    };
    Some((pre, &rest[..host_end], &rest[host_end..]))
}

/// The URL a remote host's page loads: its loopback addresses become
/// [`HOST_LOOPBACK`], so they reach the host and not this Mac.
pub fn to_page_url(url: &str) -> String {
    match split_host(url) {
        Some((pre, host, rest)) if is_loopback(host) => format!("{pre}{HOST_LOOPBACK}{rest}"),
        _ => url.to_owned(),
    }
}

/// The URL to show and report for a page: [`HOST_LOOPBACK`] reads as `localhost`.
pub fn from_page_url(url: &str) -> String {
    match split_host(url) {
        Some((pre, host, rest)) if host.eq_ignore_ascii_case(HOST_LOOPBACK) => format!("{pre}localhost{rest}"),
        _ => url.to_owned(),
    }
}

/// Text a page gave back (an agent's script result) with every
/// `//localhost.lmux` written as `//localhost`. The result is JSON, possibly
/// inside a JSON string, so the slashes may come escaped.
pub fn unalias_text(text: &str) -> String {
    text.replace(&format!("//{HOST_LOOPBACK}"), "//localhost")
        .replace(&format!("\\/\\/{HOST_LOOPBACK}"), "\\/\\/localhost")
        .replace(&format!("\\\\/\\\\/{HOST_LOOPBACK}"), "\\\\/\\\\/localhost")
}

/// Whether `url` names a loopback host (the Mac's own, from a remote page's view).
pub fn is_loopback_url(url: &str) -> bool {
    split_host(url).is_some_and(|(_, host, _)| is_loopback(host))
}

fn is_loopback(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h == "0.0.0.0"
        || h == "[::1]"
        || h.split('.').next() == Some("127") && h.split('.').count() == 4 && h.split('.').all(|p| p.parse::<u8>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_urls_point_at_the_host() {
        assert_eq!(to_page_url("http://localhost:3000/a?b#c"), "http://localhost.lmux:3000/a?b#c");
        assert_eq!(to_page_url("http://127.0.0.1:8080"), "http://localhost.lmux:8080");
        assert_eq!(to_page_url("http://[::1]:5173/"), "http://localhost.lmux:5173/");
        assert_eq!(to_page_url("http://app.localhost/"), "http://localhost.lmux/");
        assert_eq!(to_page_url("https://example.com/localhost"), "https://example.com/localhost");
        assert_eq!(to_page_url("about:blank"), "about:blank");
        assert_eq!(to_page_url("http://127.0.0.1.example.com/"), "http://127.0.0.1.example.com/");
        assert_eq!(from_page_url("http://localhost.lmux:3000/x"), "http://localhost:3000/x");
        assert_eq!(from_page_url("https://example.com/"), "https://example.com/");
        assert_eq!(unalias_text(r#"{"url":"http://localhost.lmux:3000/"}"#), r#"{"url":"http://localhost:3000/"}"#);
        assert_eq!(unalias_text(r#"{\"u\":\"http:\\/\\/localhost.lmux:1\"}"#), r#"{\"u\":\"http:\\/\\/localhost:1\"}"#);
        assert_eq!(unalias_text(r#"http:\/\/localhost.lmux:1"#), r#"http:\/\/localhost:1"#);
    }

    /// A fake upstream SOCKS server that records the requested target and
    /// then echoes bytes back.
    fn fake_upstream() -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut g = [0u8; 3];
                s.read_exact(&mut g).unwrap();
                s.write_all(&[5, 0]).unwrap();
                let mut h = [0u8; 5];
                s.read_exact(&mut h).unwrap();
                let mut name = vec![0u8; h[4] as usize + 2];
                s.read_exact(&mut name).unwrap();
                tx.send(name.clone()).unwrap();
                s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap();
                s.write_all(&buf[..n]).unwrap();
            }
        });
        (port, rx)
    }

    #[test]
    fn relays_and_maps_the_host_loopback_name() {
        let (up, seen) = fake_upstream();
        let relay = Relay::start(0).unwrap();
        relay.set_upstream(up);
        let mut c = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        c.write_all(&[5, 1, 0]).unwrap();
        let mut r = [0u8; 2];
        c.read_exact(&mut r).unwrap();
        assert_eq!(r, [5, 0]);
        let mut req = vec![5, 1, 0, 3, HOST_LOOPBACK.len() as u8];
        req.extend_from_slice(HOST_LOOPBACK.as_bytes());
        req.extend_from_slice(&3000u16.to_be_bytes());
        c.write_all(&req).unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).unwrap();
        assert_eq!(rep[1], 0, "relay reports success");
        let asked = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(&asked[..asked.len() - 2], b"localhost");
        assert_eq!(&asked[asked.len() - 2..], &3000u16.to_be_bytes());
        c.write_all(b"ping").unwrap();
        let mut echo = [0u8; 4];
        c.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"ping");
    }

    #[test]
    fn host_only_names_go_through_the_host() {
        let name = |n: &str| host_only(&Target::Name(n.into()));
        assert!(name("localhost"));
        assert!(name("myserver"));
        assert!(name("box.tail1234.ts.net"));
        assert!(name("printer.local"));
        assert!(name("10.0.0.5"));
        assert!(name("100.64.0.2"));
        assert!(!name("example.com"));
        assert!(!name("8.8.8.8"));
        assert!(host_only(&Target::V4([192, 168, 1, 2])));
        assert!(!host_only(&Target::V4([1, 1, 1, 1])));
    }

    #[test]
    fn mapped_nat64_and_reserved_addresses_are_host_only() {
        let v6 = |s: &str| host_only(&Target::V6(s.parse::<Ipv6Addr>().unwrap().octets()));
        assert!(v6("::ffff:127.0.0.1"));
        assert!(v6("::ffff:10.1.2.3"));
        assert!(v6("64:ff9b::7f00:1"));
        assert!(v6("64:ff9b::a00:1"));
        assert!(!v6("64:ff9b::808:808"));
        assert!(!v6("::ffff:8.8.8.8"));
        assert!(host_only(&Target::V4([0, 1, 2, 3])));
        assert!(host_only(&Target::V4([198, 18, 0, 1])));
        assert!(host_only(&Target::V4([198, 19, 255, 1])));
        assert!(host_only(&Target::V4([255, 255, 255, 255])));
        assert!(!host_only(&Target::V4([198, 20, 0, 1])));
        assert!(host_only(&Target::Name("::ffff:127.0.0.1".into())));
    }

    fn socks_connect(relay: &Relay, name: &str, port: u16) -> u8 {
        let mut c = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        c.write_all(&[5, 1, 0]).unwrap();
        let mut r = [0u8; 2];
        c.read_exact(&mut r).unwrap();
        let mut req = vec![5, 1, 0, 3, name.len() as u8];
        req.extend_from_slice(name.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        c.write_all(&req).unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).unwrap();
        rep[1]
    }

    #[test]
    fn the_host_daemon_port_is_not_reachable_on_the_hosts_loopback() {
        let (up, seen) = fake_upstream();
        let relay = Relay::start(7878).unwrap();
        relay.set_upstream(up);
        for name in [HOST_LOOPBACK, "localhost", "127.0.0.1", "foo.localhost", "::ffff:127.0.0.1", "0.0.0.0", "myhost"] {
            assert_ne!(socks_connect(&relay, name, 7878), 0, "{name}:7878 must be refused");
        }
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err(), "nothing reached the host");
        // Other ports on the host's loopback still work, and so does a changed daemon port.
        assert_eq!(socks_connect(&relay, HOST_LOOPBACK, 3000), 0);
        relay.set_guarded_port(7898);
        assert_ne!(socks_connect(&relay, "localhost", 7898), 0);
    }

    #[test]
    fn a_public_name_that_resolves_to_loopback_here_is_refused() {
        let at = |ip: &str| {
            let ip: IpAddr = ip.parse().unwrap();
            move |_: &str, p: u16| Ok(vec![std::net::SocketAddr::new(ip, p)])
        };
        let name = Target::Name("rebind.example.com".into());
        assert!(!host_only(&name), "public by the name rules");
        for ip in ["127.0.0.1", "::1", "::ffff:127.0.0.1", "0.0.0.0"] {
            assert!(matches!(decide_with(&name, 80, 0, &at(ip)), Route::Refuse), "{ip}");
        }
        // Split DNS: private (not loopback) answers go to the host by name.
        for ip in ["10.0.0.5", "169.254.1.1", "100.64.0.2"] {
            assert!(matches!(decide_with(&name, 80, 0, &at(ip)), Route::Upstream), "{ip}");
        }
        // A literal private address is host-only and never resolved.
        // One loopback answer among public ones is enough to refuse.
        let mixed = |_: &str, p: u16| Ok(vec![std::net::SocketAddr::from(([8, 8, 8, 8], p)), std::net::SocketAddr::from(([127, 0, 0, 1], p))]);
        assert!(matches!(decide_with(&name, 80, 0, &mixed), Route::Refuse));
        // A name that does not resolve here is the host's to try.
        let none = |_: &str, _: u16| Err(io::Error::other("nxdomain"));
        assert!(matches!(decide_with(&name, 80, 0, &none), Route::Upstream));
        // Host-only by name still goes to the host.
        assert!(matches!(decide_with(&Target::Name("box.local".into()), 80, 0, &none), Route::Upstream));
    }

    /// DNS may answer the Mac with NXDOMAIN or a private address and the host
    /// with 127.0.0.1: no public name may go upstream to the daemon's port.
    #[test]
    fn no_upstream_route_reaches_the_daemon_port() {
        let name = Target::Name("rebind.example.com".into());
        let none = |_: &str, _: u16| Err(io::Error::other("nxdomain"));
        let private = |_: &str, p: u16| Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 5], p))]);
        assert!(matches!(decide_with(&name, 7878, 7878, &none), Route::Refuse));
        assert!(matches!(decide_with(&name, 7878, 7878, &private), Route::Refuse));
        assert!(matches!(decide_with(&name, 7879, 7878, &none), Route::Upstream), "other ports still work");
        assert!(matches!(decide_with(&name, 7878, 0, &none), Route::Upstream), "no guard configured");
    }

    /// A name that resolves here but cannot be reached from here is not retried
    /// through the host.
    #[test]
    fn a_failed_connect_does_not_fall_back_to_the_host() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = l.local_addr().unwrap().port();
        drop(l);
        let at = move |_: &str, _: u16| Ok(vec![std::net::SocketAddr::from(([203, 0, 113, 1], dead))]);
        // 203.0.113.0/24 is TEST-NET-3: public by the rules, unroutable.
        let name = Target::Name("down.example.com".into());
        assert!(!matches!(decide_with(&name, dead, 0, &at), Route::Upstream));
    }

    #[test]
    fn without_a_tunnel_the_connection_fails_cleanly() {
        let relay = Relay::start(0).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        c.write_all(&[5, 1, 0, 5, 1, 0, 1, 10, 2, 3, 4, 0, 80]).unwrap();
        let mut buf = [0u8; 12];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..2], &[5, 0]);
        assert_eq!(buf[3], 1, "general failure");
    }
}
