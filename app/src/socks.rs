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
}

impl Relay {
    /// Listen on a free loopback port. Connections fail until an upstream is set.
    pub fn start() -> io::Result<Relay> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let upstream = Arc::new(AtomicU16::new(0));
        let up = upstream.clone();
        std::thread::Builder::new().name("lmux-socks".into()).spawn(move || {
            for conn in listener.incoming().flatten() {
                let up = up.load(Ordering::Relaxed);
                let _ = std::thread::Builder::new().name("lmux-socks-conn".into()).spawn(move || {
                    let _ = relay(conn, up);
                });
            }
        })?;
        Ok(Relay { port, upstream })
    }

    pub fn port(&self) -> u16 {
        self.port
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
fn relay(mut client: TcpStream, upstream: u16) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (target, port) = read_request(&mut client)?;
    let target = map_target(target);
    let direct = if host_only(&target) { None } else { connect_direct(&target, port) };
    let up = match direct {
        Some(s) => Ok(s),
        None if upstream == 0 => Err(io::Error::new(io::ErrorKind::NotConnected, "no tunnel")),
        None => connect_upstream(upstream, &target, port),
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

/// Targets that must go through the host: its loopback, private and
/// link-local addresses, and names that are not public DNS names.
fn host_only(t: &Target) -> bool {
    match t {
        Target::V4(a) => {
            let ip = std::net::Ipv4Addr::from(*a);
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || a[0] == 100 && (a[1] & 0xC0) == 64
        }
        Target::V6(a) => {
            let ip = std::net::Ipv6Addr::from(*a);
            ip.is_loopback() || ip.is_unspecified() || (a[0] & 0xFE) == 0xFC || (a[0] == 0xFE && (a[1] & 0xC0) == 0x80)
        }
        Target::Name(n) => {
            let n = n.trim_end_matches('.').to_ascii_lowercase();
            !n.contains('.')
                || n == "localhost"
                || [".localhost", ".local", ".internal", ".lan", ".home.arpa", ".ts.net", ".corp"].iter().any(|s| n.ends_with(s))
                || n.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
                    std::net::IpAddr::V4(v) => host_only(&Target::V4(v.octets())),
                    std::net::IpAddr::V6(v) => host_only(&Target::V6(v.octets())),
                })
        }
    }
}

/// Connect from this Mac. `None` when the name does not resolve here to a
/// public address, or nothing answers: the host may still reach it.
fn connect_direct(t: &Target, port: u16) -> Option<TcpStream> {
    use std::net::{SocketAddr, ToSocketAddrs};
    let addrs: Vec<SocketAddr> = match t {
        Target::V4(a) => vec![SocketAddr::from((*a, port))],
        Target::V6(a) => vec![SocketAddr::from((*a, port))],
        Target::Name(n) => (n.as_str(), port).to_socket_addrs().ok()?.collect(),
    };
    // A name that resolves to a private address here (split DNS, VPN) is
    // the Mac's network, not the host's.
    let public = |a: &SocketAddr| match a {
        SocketAddr::V4(v) => !host_only(&Target::V4(v.ip().octets())),
        SocketAddr::V6(v) => !host_only(&Target::V6(v.ip().octets())),
    };
    addrs
        .iter()
        .filter(|a| public(a))
        .find_map(|a| TcpStream::connect_timeout(a, Duration::from_secs(5)).ok())
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
        let relay = Relay::start().unwrap();
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
    fn without_a_tunnel_the_connection_fails_cleanly() {
        let relay = Relay::start().unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        c.write_all(&[5, 1, 0, 5, 1, 0, 1, 10, 2, 3, 4, 0, 80]).unwrap();
        let mut buf = [0u8; 12];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..2], &[5, 0]);
        assert_eq!(buf[3], 1, "general failure");
    }
}
