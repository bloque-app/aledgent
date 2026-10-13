// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The relay's authenticated transport: Noise `IK` over TCP, with static keys pinned by each host.
//!
//! Every host draws an X25519 static key ([`Keypair`]) and publishes its public half; a host
//! dialling a peer must know the peer's public key (`IK`'s `s` pre-message), and a host answering
//! learns the dialler's from the handshake and accepts it only if it is one of the book's hosts
//! ([`Accept`]). Both sides are authenticated by Diffie–Hellman with their static keys (`es`, `ss`,
//! `se`), and the handshake's prologue binds the session to one book's relay (`net id`), so a
//! session of another book or run cannot be replayed into this one.
//!
//! After the handshake the stream carries **frames** (one relay message each): a frame is a
//! big-endian `u32` length and the bytes, cut into Noise transport messages of at most
//! [`MAX_PLAIN`] bytes, each sent as a big-endian `u16` length and the ciphertext. ChaCha20-Poly1305
//! authenticates every message under its nonce, so a dropped, replayed, reordered or altered
//! message ends the session; the relay's streams recover across a new session (`net::node`).

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::string::{String, ToString};
use std::sync::Arc;
use std::vec;
use std::vec::Vec;

use snow::{Builder, StatelessTransportState};
use zeroize::Zeroize;

/// The Noise protocol of the relay.
pub const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// The most plaintext one Noise transport message carries (65 535 less the 16-byte tag).
pub const MAX_PLAIN: usize = 65_535 - 16;
/// The largest frame the relay accepts: a report or a recovery may be large, never this large.
pub const MAX_FRAME: usize = 64 << 20;
const PROLOGUE: &[u8] = b"aledgent/relay/v1";

fn params() -> snow::params::NoiseParams {
    PATTERN.parse().expect("a valid Noise pattern")
}

fn err(e: impl core::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// A host's static key: a test value in every harness here, never key material of the book.
#[derive(Clone)]
pub struct Keypair {
    secret: [u8; 32],
    /// The public half: what peers pin.
    pub public: [u8; 32],
}

impl Drop for Keypair {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl core::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Keypair {{ public: {} }}", hex(&self.public))
    }
}

impl Keypair {
    /// A fresh key from the OS's random source.
    pub fn generate() -> Self {
        let k = Builder::new(params())
            .generate_keypair()
            .expect("the OS random source");
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&k.private);
        let mut public = [0u8; 32];
        public.copy_from_slice(&k.public);
        Keypair { secret, public }
    }

    /// The key whose secret half is `secret`.
    pub fn from_secret(secret: [u8; 32]) -> Self {
        // X25519: the public half is the base point times the clamped scalar; snow derives it the
        // same way when given the private key, so take it from a handshake-free derivation.
        let public = x25519_public(&secret);
        Keypair { secret, public }
    }

    /// The secret half, hex, for a key file (`aledgent-core-host fleet-key`).
    pub fn secret_hex(&self) -> String {
        hex(&self.secret)
    }
}

/// X25519's public key of `secret`, through snow's own DH (no second curve implementation).
fn x25519_public(secret: &[u8; 32]) -> [u8; 32] {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut dh = DefaultResolver
        .resolve_dh(&snow::params::DHChoice::Curve25519)
        .expect("X25519");
    dh.set(secret);
    let mut out = [0u8; 32];
    out.copy_from_slice(dh.pubkey());
    out
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// 32 bytes from 64 hex digits.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Whom an answering host accepts: the dialler's static public key, mapped to its host index.
pub type Accept = Arc<dyn Fn(&[u8; 32]) -> Option<u16> + Send + Sync>;

/// One side of an established session.
pub struct Session {
    /// The peer's host index.
    pub peer: u16,
    /// Writes frames.
    pub writer: Writer,
    /// Reads frames.
    pub reader: Reader,
}

fn write_msg(s: &mut TcpStream, m: &[u8]) -> io::Result<()> {
    let len = u16::try_from(m.len()).map_err(err)?;
    let mut b = Vec::with_capacity(2 + m.len());
    b.extend_from_slice(&len.to_be_bytes());
    b.extend_from_slice(m);
    s.write_all(&b)
}

fn read_msg(s: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut l = [0u8; 2];
    s.read_exact(&mut l)?;
    buf.resize(usize::from(u16::from_be_bytes(l)), 0);
    s.read_exact(buf)
}

fn prologue(net_id: &[u8]) -> Vec<u8> {
    let mut p = PROLOGUE.to_vec();
    p.extend_from_slice(&(net_id.len() as u32).to_be_bytes());
    p.extend_from_slice(net_id);
    p
}

fn session(stream: TcpStream, ts: StatelessTransportState, peer: u16) -> io::Result<Session> {
    stream.set_nodelay(true)?;
    let ts = Arc::new(ts);
    let read = stream.try_clone()?;
    Ok(Session {
        peer,
        writer: Writer {
            stream,
            ts: Arc::clone(&ts),
            nonce: 0,
            buf: vec![0u8; 65_535],
        },
        reader: Reader {
            stream: read,
            ts,
            nonce: 0,
            msg: Vec::new(),
            plain: vec![0u8; 65_535],
            pending: Vec::new(),
        },
    })
}

/// Dial: `stream` is connected to host `peer`, whose static public key is `remote`. `me_index`
/// travels in the first handshake message's payload, under the handshake's encryption.
pub fn initiate(
    mut stream: TcpStream,
    me: &Keypair,
    me_index: u16,
    peer: u16,
    remote: &[u8; 32],
    net_id: &[u8],
) -> io::Result<Session> {
    let p = prologue(net_id);
    let mut hs = Builder::new(params())
        .local_private_key(&me.secret)
        .and_then(|b| b.remote_public_key(remote))
        .and_then(|b| b.prologue(&p))
        .and_then(Builder::build_initiator)
        .map_err(err)?;
    let mut buf = vec![0u8; 1024];
    let n = hs
        .write_message(&me_index.to_be_bytes(), &mut buf)
        .map_err(err)?;
    write_msg(&mut stream, &buf[..n])?;
    let mut msg = Vec::new();
    read_msg(&mut stream, &mut msg)?;
    let mut payload = vec![0u8; 1024];
    let n = hs.read_message(&msg, &mut payload).map_err(err)?;
    if payload[..n] != peer.to_be_bytes() {
        return Err(err("the responder is not the host dialled"));
    }
    let ts = hs.into_stateless_transport_mode().map_err(err)?;
    session(stream, ts, peer)
}

/// Answer a dialler on `stream`: accepted only if its static key is one `accept` knows, and
/// only as the host index that key belongs to.
pub fn respond(
    mut stream: TcpStream,
    me: &Keypair,
    me_index: u16,
    accept: &Accept,
    net_id: &[u8],
) -> io::Result<Session> {
    let p = prologue(net_id);
    let mut hs = Builder::new(params())
        .local_private_key(&me.secret)
        .and_then(|b| b.prologue(&p))
        .and_then(Builder::build_responder)
        .map_err(err)?;
    let mut msg = Vec::new();
    read_msg(&mut stream, &mut msg)?;
    let mut payload = vec![0u8; 1024];
    let n = hs.read_message(&msg, &mut payload).map_err(err)?;
    let mut remote = [0u8; 32];
    remote.copy_from_slice(hs.get_remote_static().ok_or_else(|| err("no static key"))?);
    let peer = accept(&remote).ok_or_else(|| err("an unknown static key"))?;
    if payload[..n] != peer.to_be_bytes() {
        return Err(err("the dialler claims another host's index"));
    }
    let mut buf = vec![0u8; 1024];
    let n = hs
        .write_message(&me_index.to_be_bytes(), &mut buf)
        .map_err(err)?;
    write_msg(&mut stream, &buf[..n])?;
    let ts = hs.into_stateless_transport_mode().map_err(err)?;
    session(stream, ts, peer)
}

/// The sending half of a session.
pub struct Writer {
    stream: TcpStream,
    ts: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

impl Writer {
    /// Send one frame.
    pub fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        if frame.len() > MAX_FRAME {
            return Err(err("frame too large"));
        }
        let mut plain = Vec::with_capacity(4 + frame.len());
        plain.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        plain.extend_from_slice(frame);
        let mut wire = Vec::with_capacity(plain.len() + plain.len() / MAX_PLAIN * 18 + 18);
        for chunk in plain.chunks(MAX_PLAIN) {
            let n = self
                .ts
                .write_message(self.nonce, chunk, &mut self.buf)
                .map_err(err)?;
            self.nonce += 1;
            wire.extend_from_slice(&(n as u16).to_be_bytes());
            wire.extend_from_slice(&self.buf[..n]);
        }
        self.stream.write_all(&wire)
    }

    /// End the session (both halves).
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// A handle that ends the session from elsewhere.
    pub fn closer(&self) -> io::Result<TcpStream> {
        self.stream.try_clone()
    }
}

/// The receiving half of a session.
pub struct Reader {
    stream: TcpStream,
    ts: Arc<StatelessTransportState>,
    nonce: u64,
    msg: Vec<u8>,
    plain: Vec<u8>,
    pending: Vec<u8>,
}

impl Reader {
    /// The next frame.
    pub fn recv(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if self.pending.len() >= 4 {
                let mut l = [0u8; 4];
                l.copy_from_slice(&self.pending[..4]);
                let len = u32::from_be_bytes(l) as usize;
                if len > MAX_FRAME {
                    return Err(err("frame too large"));
                }
                if self.pending.len() >= 4 + len {
                    let frame = self.pending[4..4 + len].to_vec();
                    self.pending.drain(..4 + len);
                    return Ok(frame);
                }
            }
            read_msg(&mut self.stream, &mut self.msg)?;
            let n = self
                .ts
                .read_message(self.nonce, &self.msg, &mut self.plain)
                .map_err(err)?;
            self.nonce += 1;
            self.pending.extend_from_slice(&self.plain[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair(
        a: &Keypair,
        b: &Keypair,
        net_a: &'static [u8],
        net_b: &'static [u8],
        accept: Accept,
    ) -> (io::Result<Session>, io::Result<Session>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let bk = b.clone();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            respond(s, &bk, 0, &accept, net_b)
        });
        let s = TcpStream::connect(addr).unwrap();
        let a_side = initiate(s, a, 1, 0, &b.public, net_a);
        (a_side, h.join().unwrap())
    }

    #[test]
    fn a_session_carries_frames_of_any_size_both_ways() {
        let (a, b) = (Keypair::generate(), Keypair::generate());
        let ap = a.public;
        let accept: Accept = Arc::new(move |k| (*k == ap).then_some(1));
        let (sa, sb) = pair(&a, &b, b"book", b"book", accept);
        let (mut sa, mut sb) = (sa.unwrap(), sb.unwrap());
        assert_eq!((sa.peer, sb.peer), (0, 1));
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        for f in [&b"x"[..], &[][..], &big[..]] {
            sa.writer.send(f).unwrap();
            assert_eq!(sb.reader.recv().unwrap(), f);
            sb.writer.send(f).unwrap();
            assert_eq!(sa.reader.recv().unwrap(), f);
        }
    }

    #[test]
    fn an_unknown_key_or_another_book_is_refused() {
        let (a, b, c) = (
            Keypair::generate(),
            Keypair::generate(),
            Keypair::generate(),
        );
        let cp = c.public;
        let only_c: Accept = Arc::new(move |k| (*k == cp).then_some(1));
        let (_, sb) = pair(&a, &b, b"book", b"book", only_c);
        assert!(sb.is_err(), "a key the responder does not know");
        let ap = a.public;
        let accept: Accept = Arc::new(move |k| (*k == ap).then_some(1));
        let (sa, sb) = pair(&a, &b, b"book-1", b"book-2", accept);
        assert!(sa.is_err() || sb.is_err(), "another book's session");
        let wrong = Keypair::generate();
        let accept: Accept = Arc::new(move |k| (*k == ap).then_some(1));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let bk = b.clone();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            respond(s, &bk, 0, &accept, b"book")
        });
        let s = TcpStream::connect(addr).unwrap();
        assert!(
            initiate(s, &a, 1, 0, &wrong.public, b"book").is_err(),
            "a responder that is not the host pinned"
        );
        let _ = h.join();
    }

    #[test]
    fn a_key_round_trips_through_its_secret() {
        let k = Keypair::generate();
        let again = Keypair::from_secret(unhex32(&k.secret_hex()).unwrap());
        assert_eq!(again.public, k.public);
    }
}
