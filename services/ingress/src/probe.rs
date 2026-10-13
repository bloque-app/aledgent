// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A container's health check, from inside an image with no shell and no HTTP client (the runtime
//! images are distroless): one `GET` to this host's own listener, and the status it answers.
//!
//! `aledgent-ingress health` and `opsctl health` use it as their image's `HEALTHCHECK`
//! (deploy/docker/services.Dockerfile), so that an orchestrator, and podman's auto-update, can tell
//! an update that came up from one that did not, and roll the latter back. Blocking and std-only:
//! the check is its own short-lived process.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The address to probe for a listen address (`host:port`): an unspecified host (`0.0.0.0`, `[::]`),
/// which a server binds to listen on every interface, is probed on loopback.
pub fn address(listen: &str) -> Result<SocketAddr, String> {
    let addr = listen
        .to_socket_addrs()
        .map_err(|e| format!("{listen}: {e}"))?
        .next()
        .ok_or_else(|| format!("{listen}: no address"))?;
    Ok(if addr.ip().is_unspecified() {
        let loopback: std::net::IpAddr = if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        };
        SocketAddr::new(loopback, addr.port())
    } else {
        addr
    })
}

/// `GET <path>` at `addr`, with no credential; the status code of the answer. An error when nothing
/// answers within `timeout`, or what answers is not HTTP/1.
pub fn status(addr: SocketAddr, path: &str, timeout: Duration) -> Result<u16, String> {
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("{addr}: {e}"))?;
    s.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(request.as_bytes())
        .map_err(|e| format!("{addr}: {e}"))?;
    // The status line, `HTTP/1.x NNN …`: its first twelve bytes are enough.
    let mut head = [0u8; 12];
    s.read_exact(&mut head)
        .map_err(|e| format!("{addr}: {e}"))?;
    let line = String::from_utf8_lossy(&head);
    let code = line
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.get(2..5))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("{addr}: not an HTTP/1 answer: {:?}", line.trim()))?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A listener that answers one request with `answer`, on a thread.
    fn answering(answer: &'static str) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = c.read(&mut buf);
            let _ = c.write_all(answer.as_bytes());
        });
        addr
    }

    #[test]
    fn the_status_of_an_answer_is_read() {
        let t = Duration::from_secs(3);
        let ok = answering("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(status(ok, "/metrics", t), Ok(200));
        let refused = answering("HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(status(refused, "/openapi.json", t), Ok(401));
    }

    #[test]
    fn what_is_not_http_is_an_error() {
        let t = Duration::from_secs(3);
        assert!(status(answering("SSH-2.0-OpenSSH_9.6\r\n"), "/", t).is_err());
        // Nothing listening: refused at once.
        let free = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(status(free, "/", t).is_err());
    }

    #[test]
    fn an_unspecified_host_is_probed_on_loopback() {
        assert_eq!(
            address("0.0.0.0:8080").unwrap(),
            "127.0.0.1:8080".parse().unwrap()
        );
        assert_eq!(address("[::]:9464").unwrap(), "[::1]:9464".parse().unwrap());
        assert_eq!(
            address("127.0.0.1:1").unwrap(),
            "127.0.0.1:1".parse().unwrap()
        );
    }
}
