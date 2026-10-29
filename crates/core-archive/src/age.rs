// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! age v1 (`age-encryption.org/v1`, C2SP), the parts the archive's seal uses (`F-044`).
//!
//! The sealed part of every archived object is an age file, so that the offline recovery key opens
//! it with the stock `age` tool and nothing of aledgent's (`features/F-044-archive-sealing`):
//!
//! - the **header**: the version line, one stanza per recipient, and a MAC under the file key;
//! - the **`X25519` stanza** (the recovery key), exactly as `age` writes it;
//! - **any other stanza** as opaque type, arguments and body: the seal adds `aledgent-transit`
//!   (the file key wrapped to a Vault Transit key) and `aledgent-seal` (the digest of the seal's
//!   clear header, so the header MAC covers it). `age` ignores stanzas it does not know;
//! - the **payload**: a 16-byte nonce, then STREAM over ChaCha20-Poly1305 in 64 KiB chunks.
//!
//! Not here: passphrases (`scrypt`), SSH recipients, armor, plugins. Nothing here reads a key from
//! the environment or prints one: identities and file keys never appear in an error or in `Debug`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use chacha20poly1305::aead::{Aead as _, KeyInit as _};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore as _};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

/// The version line.
pub const VERSION_LINE: &str = "age-encryption.org/v1";

/// A file key: 16 bytes, wiped when dropped.
pub type FileKey = Zeroizing<[u8; 16]>;

/// Bytes of plaintext in one payload chunk.
pub const CHUNK: usize = 64 * 1024;

/// The tag ChaCha20-Poly1305 adds.
const TAG: usize = 16;

/// The most bytes a header may take: the seal's has three or four stanzas, each well under 1 KiB.
pub const MAX_HEADER: usize = 64 * 1024;

/// Why an age file did not open or could not be made. Never carries key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgeError(pub String);

impl core::fmt::Display for AgeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "age: {}", self.0)
    }
}

fn err(s: impl Into<String>) -> AgeError {
    AgeError(s.into())
}

/// A fresh random file key.
pub fn new_file_key() -> FileKey {
    let mut k = Zeroizing::new([0u8; 16]);
    OsRng.fill_bytes(k.as_mut());
    k
}

/// HKDF-SHA-256 (RFC 5869), 32 bytes out.
fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, out.as_mut())
        .unwrap_or_else(|_| unreachable!("32 bytes is a valid HKDF-SHA-256 length"));
    out
}

/// One recipient stanza: `-> type args…`, then its body.
#[derive(Clone, PartialEq, Eq)]
pub struct Stanza {
    /// Its type (`X25519`, `aledgent-transit`, …).
    pub kind: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its body.
    pub body: Vec<u8>,
}

impl core::fmt::Debug for Stanza {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Stanza")
            .field("kind", &self.kind)
            .field("args", &self.args)
            .field("body_len", &self.body.len())
            .finish()
    }
}

fn vchar(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

impl Stanza {
    /// Its text in a header: the arguments line, and the body in base64 lines of 64 columns, the
    /// last one shorter (empty when the body's base64 is a multiple of 64).
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"-> ");
        out.extend_from_slice(self.kind.as_bytes());
        for a in &self.args {
            out.push(b' ');
            out.extend_from_slice(a.as_bytes());
        }
        out.push(b'\n');
        let b64 = STANDARD_NO_PAD.encode(&self.body);
        let mut rest = b64.as_bytes();
        loop {
            let (line, more) = rest.split_at(rest.len().min(64));
            out.extend_from_slice(line);
            out.push(b'\n');
            if line.len() < 64 {
                break;
            }
            rest = more;
        }
    }
}

/// A parsed header.
#[derive(Debug)]
pub struct Header {
    /// Its stanzas, in order.
    pub stanzas: Vec<Stanza>,
    /// The bytes the MAC covers: from the version line through `---`.
    pub mac_input: Vec<u8>,
    /// The MAC.
    pub mac: [u8; 32],
}

fn header_mac(file_key: &[u8; 16], mac_input: &[u8]) -> [u8; 32] {
    let key = hkdf(&[], file_key, b"header");
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key.as_ref())
        .unwrap_or_else(|_| unreachable!("HMAC takes any key"));
    m.update(mac_input);
    m.finalize().into_bytes().into()
}

/// The header for `stanzas` under `file_key`, MAC included.
pub fn encode_header(file_key: &[u8; 16], stanzas: &[Stanza]) -> Result<Vec<u8>, AgeError> {
    let mut out = Vec::new();
    out.extend_from_slice(VERSION_LINE.as_bytes());
    out.push(b'\n');
    for s in stanzas {
        if !vchar(&s.kind) || !s.args.iter().all(|a| vchar(a)) {
            return Err(err("a stanza's type or argument is not printable ASCII"));
        }
        s.encode(&mut out);
    }
    out.extend_from_slice(b"---");
    let mac = header_mac(file_key, &out);
    out.push(b' ');
    out.extend_from_slice(STANDARD_NO_PAD.encode(mac).as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// The next `\n`-terminated line of `input` at `at`, without its `\n`, and where the next starts.
fn line(input: &[u8], at: usize) -> Result<(&[u8], usize), AgeError> {
    let rest = input.get(at..).ok_or_else(|| err("truncated header"))?;
    let end = rest
        .iter()
        .take(MAX_HEADER)
        .position(|b| *b == b'\n')
        .ok_or_else(|| err("truncated header"))?;
    Ok((&rest[..end], at + end + 1))
}

fn b64(s: &[u8]) -> Result<Vec<u8>, AgeError> {
    STANDARD_NO_PAD
        .decode(s)
        .map_err(|_| err("a header's base64 is not canonical"))
}

/// Parse the header at the start of `input`; the payload starts at the returned offset. The MAC
/// is not checked here: [`check_mac`] needs the file key.
pub fn parse_header(input: &[u8]) -> Result<(Header, usize), AgeError> {
    let (v, mut at) = line(input, 0)?;
    if v != VERSION_LINE.as_bytes() {
        return Err(err("not an age v1 file"));
    }
    let mut stanzas = Vec::new();
    loop {
        if at > MAX_HEADER {
            return Err(err("header too large"));
        }
        let (l, next) = line(input, at)?;
        if let Some(mac) = l.strip_prefix(b"--- ") {
            let mac: [u8; 32] = b64(mac)?.try_into().map_err(|_| err("a MAC is 32 bytes"))?;
            let mac_input = input[..at + 3].to_vec();
            return Ok((
                Header {
                    stanzas,
                    mac_input,
                    mac,
                },
                next,
            ));
        }
        let args = l
            .strip_prefix(b"-> ")
            .ok_or_else(|| err("expected a stanza or the MAC line"))?;
        let args = core::str::from_utf8(args).map_err(|_| err("a stanza line is not ASCII"))?;
        let mut parts = args.split(' ');
        let kind = parts.next().unwrap_or_default().to_owned();
        let args: Vec<String> = parts.map(str::to_owned).collect();
        if !vchar(&kind) || !args.iter().all(|a| vchar(a)) {
            return Err(err("a stanza's type or argument is malformed"));
        }
        at = next;
        let mut body_b64 = Vec::new();
        loop {
            let (l, next) = line(input, at)?;
            if l.len() > 64 {
                return Err(err("a stanza's body line is longer than 64 columns"));
            }
            body_b64.extend_from_slice(l);
            at = next;
            if l.len() < 64 {
                break;
            }
        }
        stanzas.push(Stanza {
            kind,
            args,
            body: b64(&body_b64)?,
        });
    }
}

/// Check `header`'s MAC under `file_key`.
pub fn check_mac(header: &Header, file_key: &[u8; 16]) -> Result<(), AgeError> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(hkdf(&[], file_key, b"header").as_ref())
        .unwrap_or_else(|_| unreachable!("HMAC takes any key"));
    m.update(&header.mac_input);
    m.verify_slice(&header.mac)
        .map_err(|_| err("the header's MAC does not verify under this file key"))
}

fn chunk_nonce(counter: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[3..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = u8::from(last);
    n
}

/// The payload of `plaintext` under `file_key`: a fresh nonce, then STREAM.
pub fn encrypt_payload(file_key: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    let key = hkdf(&nonce, file_key, b"payload");
    let aead = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let chunks = plaintext.len().div_ceil(CHUNK).max(1);
    let mut out = Vec::with_capacity(16 + plaintext.len() + chunks * TAG);
    out.extend_from_slice(&nonce);
    for i in 0..chunks {
        let start = i * CHUNK;
        let part = &plaintext[start..plaintext.len().min(start + CHUNK)];
        let n = chunk_nonce(i as u64, i + 1 == chunks);
        let ct = aead
            .encrypt(Nonce::from_slice(&n), part)
            .unwrap_or_else(|_| unreachable!("ChaCha20-Poly1305 encrypts any 64 KiB"));
        out.extend_from_slice(&ct);
    }
    out
}

/// Open a payload under `file_key`; refuses more than `max` bytes of plaintext.
pub fn decrypt_payload(file_key: &[u8; 16], payload: &[u8], max: u64) -> Result<Vec<u8>, AgeError> {
    let (nonce, mut rest) = payload
        .split_at_checked(16)
        .ok_or_else(|| err("truncated payload"))?;
    let key = hkdf(nonce, file_key, b"payload");
    let aead = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let mut out = Vec::new();
    let mut counter = 0u64;
    loop {
        let last = rest.len() <= CHUNK + TAG;
        if rest.len() < TAG {
            return Err(err("truncated payload"));
        }
        let (ct, more) = rest.split_at(rest.len().min(CHUNK + TAG));
        let pt = aead
            .decrypt(Nonce::from_slice(&chunk_nonce(counter, last)), ct)
            .map_err(|_| err("a payload chunk does not open under this file key"))?;
        if last && pt.is_empty() && counter > 0 {
            return Err(err("an empty last chunk after others"));
        }
        if (out.len() + pt.len()) as u64 > max {
            return Err(err("the payload is larger than its bound"));
        }
        out.extend_from_slice(&pt);
        if last {
            return Ok(out);
        }
        rest = more;
        counter += 1;
    }
}

// --- X25519 recipients and identities, and their Bech32 encoding --------------------------------

const BECH32: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [
        0x3b6a_57b2,
        0x2650_8e6d,
        0x1ea1_19fa,
        0x3d42_33dd,
        0x2a14_62b3,
    ];
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ u32::from(*v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut v: Zeroizing<Vec<u8>> = Zeroizing::new(hrp.iter().map(|c| c >> 5).collect());
    v.push(0);
    v.extend(hrp.iter().map(|c| c & 31));
    v
}

fn convert_bits(data: &[u8], from: u32, to: u32, pad: bool) -> Option<Zeroizing<Vec<u8>>> {
    let (mut acc, mut bits) = (0u32, 0u32);
    let maxv = (1u32 << to) - 1;
    let mut out = Zeroizing::new(Vec::new());
    for &d in data {
        let d = u32::from(d);
        if d >> from != 0 {
            return None;
        }
        acc = (acc << from) | d;
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & maxv) as u8);
        }
    }
    if pad {
        if bits > 0 {
            out.push(((acc << (to - bits)) & maxv) as u8);
        }
    } else if bits >= from || ((acc << (to - bits)) & maxv) != 0 {
        return None;
    }
    Some(out)
}

/// Bech32 (BIP 173) of `data` under `hrp`, lower case. Every buffer it uses is wiped (an
/// identity's secret passes through it).
pub fn bech32_encode(hrp: &str, data: &[u8]) -> Zeroizing<String> {
    let hrp = hrp.to_ascii_lowercase();
    let five = convert_bits(data, 8, 5, true).unwrap_or_default();
    let mut values = hrp_expand(hrp.as_bytes());
    values.extend_from_slice(&five);
    values.extend_from_slice(&[0; 6]);
    let pm = polymod(&values) ^ 1;
    let mut s = Zeroizing::new(String::with_capacity(hrp.len() + 1 + five.len() + 6));
    s.push_str(&hrp);
    s.push('1');
    for v in five.iter() {
        s.push(BECH32[*v as usize] as char);
    }
    for i in 0..6 {
        s.push(BECH32[((pm >> (5 * (5 - i))) & 31) as usize] as char);
    }
    s
}

/// Decode Bech32 (BIP 173): (the HRP in lower case, the data). One case throughout. Every buffer
/// it uses is wiped.
pub fn bech32_decode(s: &str) -> Option<(String, Zeroizing<Vec<u8>>)> {
    if s.chars().any(|c| c.is_ascii_lowercase()) && s.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let s = Zeroizing::new(s.to_ascii_lowercase());
    let pos = s.rfind('1')?;
    let (hrp, data) = (&s[..pos], &s[pos + 1..]);
    if hrp.is_empty() || data.len() < 6 {
        return None;
    }
    let values: Zeroizing<Vec<u8>> = Zeroizing::new(
        data.bytes()
            .map(|c| BECH32.iter().position(|b| *b == c).map(|p| p as u8))
            .collect::<Option<_>>()?,
    );
    let mut check = hrp_expand(hrp.as_bytes());
    check.extend_from_slice(&values);
    if polymod(&check) != 1 {
        return None;
    }
    let bytes = convert_bits(&values[..values.len() - 6], 5, 8, false)?;
    Some((hrp.to_owned(), bytes))
}

/// The `X25519` stanza's type.
pub const X25519_KIND: &str = "X25519";
const X25519_INFO: &[u8] = b"age-encryption.org/v1/X25519";

/// An age X25519 recipient (`age1…`): the recovery key's public half.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Recipient(pub [u8; 32]);

impl core::fmt::Debug for Recipient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Recipient({})", self.to_bech32())
    }
}

impl Recipient {
    /// Parse `age1…`.
    pub fn parse(s: &str) -> Result<Recipient, AgeError> {
        match bech32_decode(s.trim()) {
            Some((hrp, data)) if hrp == "age" && data.len() == 32 => {
                let mut k = [0u8; 32];
                k.copy_from_slice(&data);
                Ok(Recipient(k))
            }
            _ => Err(err("a recipient is age1… (Bech32, 32 bytes)")),
        }
    }

    /// Its `age1…` form.
    pub fn to_bech32(&self) -> String {
        bech32_encode("age", &self.0).to_string()
    }

    /// Wrap `file_key` to it: a fresh ephemeral share, as `age` does.
    pub fn wrap(&self, file_key: &[u8; 16]) -> Result<Stanza, AgeError> {
        let ephemeral = StaticSecret::random_from_rng(OsRng);
        let share = PublicKey::from(&ephemeral);
        let shared = ephemeral.diffie_hellman(&PublicKey::from(self.0));
        if !shared.was_contributory() {
            return Err(err("the recipient is a low-order point"));
        }
        let mut salt = Vec::with_capacity(64);
        salt.extend_from_slice(share.as_bytes());
        salt.extend_from_slice(&self.0);
        let key = hkdf(&salt, shared.as_bytes(), X25519_INFO);
        let body = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()))
            .encrypt(Nonce::from_slice(&[0u8; 12]), file_key.as_slice())
            .map_err(|_| err("the file key did not wrap"))?;
        Ok(Stanza {
            kind: X25519_KIND.into(),
            args: vec![STANDARD_NO_PAD.encode(share.as_bytes())],
            body,
        })
    }
}

/// An age X25519 identity (`AGE-SECRET-KEY-1…`): the recovery key. `Debug` shows only its
/// recipient.
pub struct Identity(StaticSecret);

impl core::fmt::Debug for Identity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Identity(for {})", self.recipient().to_bech32())
    }
}

impl Identity {
    /// A new one (tests, and `age-keygen`'s job otherwise).
    pub fn generate() -> Identity {
        Identity(StaticSecret::random_from_rng(OsRng))
    }

    /// Parse `AGE-SECRET-KEY-1…`. The error never quotes it.
    pub fn parse(s: &str) -> Result<Identity, AgeError> {
        match bech32_decode(s.trim()) {
            Some((hrp, data)) if hrp == "age-secret-key-" && data.len() == 32 => {
                let mut k = Zeroizing::new([0u8; 32]);
                k.copy_from_slice(&data);
                Ok(Identity(StaticSecret::from(*k)))
            }
            _ => Err(err("an identity is AGE-SECRET-KEY-1… (Bech32, 32 bytes)")),
        }
    }

    /// Every identity in an `age-keygen` file: one `AGE-SECRET-KEY-1…` a line, `#` comments.
    pub fn parse_file(text: &str) -> Result<Vec<Identity>, AgeError> {
        let ids: Vec<Identity> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(Identity::parse)
            .collect::<Result<_, _>>()?;
        if ids.is_empty() {
            return Err(err("the identity file holds no AGE-SECRET-KEY-1… line"));
        }
        Ok(ids)
    }

    /// Its `AGE-SECRET-KEY-1…` form (upper case, as `age-keygen` writes it).
    pub fn to_bech32(&self) -> Zeroizing<String> {
        let secret = Zeroizing::new(self.0.to_bytes());
        Zeroizing::new(bech32_encode("age-secret-key-", secret.as_ref()).to_ascii_uppercase())
    }

    /// Its recipient.
    pub fn recipient(&self) -> Recipient {
        Recipient(PublicKey::from(&self.0).to_bytes())
    }

    /// The file key of an `X25519` stanza addressed to it, or `None` if the stanza is of another
    /// type or for another recipient.
    pub fn unwrap(&self, stanza: &Stanza) -> Option<FileKey> {
        if stanza.kind != X25519_KIND || stanza.args.len() != 1 || stanza.body.len() != 32 {
            return None;
        }
        let share: [u8; 32] = STANDARD_NO_PAD
            .decode(&stanza.args[0])
            .ok()?
            .try_into()
            .ok()?;
        let shared = self.0.diffie_hellman(&PublicKey::from(share));
        if !shared.was_contributory() {
            return None;
        }
        let mut salt = Vec::with_capacity(64);
        salt.extend_from_slice(&share);
        salt.extend_from_slice(&self.recipient().0);
        let key = hkdf(&salt, shared.as_bytes(), X25519_INFO);
        let pt = Zeroizing::new(
            ChaCha20Poly1305::new(Key::from_slice(key.as_ref()))
                .decrypt(Nonce::from_slice(&[0u8; 12]), stanza.body.as_slice())
                .ok()?,
        );
        let mut k = Zeroizing::new([0u8; 16]);
        k.copy_from_slice(pt.get(..16)?);
        Some(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BIP 173's valid test vectors, and a mixed case refused.
    #[test]
    fn f_044_bech32_reads_bip173_vectors() {
        for s in [
            "A12UEL5L",
            "a12uel5l",
            "abcdef1qpzry9x8gf2tvdw0s3jn54khce6mua7lmqqqxw",
            "split1checkupstagehandshakeupstreamerranterredcaperred2y9e3w",
        ] {
            assert!(bech32_decode(s).is_some(), "{s}");
        }
        assert!(bech32_decode("A12uEL5L").is_none());
        assert!(bech32_decode("a12uel5m").is_none());
        let data = [7u8; 32];
        let s = bech32_encode("age", &data);
        let (hrp, back) = bech32_decode(&s).unwrap();
        assert_eq!((hrp.as_str(), back.as_slice()), ("age", data.as_slice()));
    }

    #[test]
    fn f_044_an_identity_round_trips_and_its_debug_hides_it() {
        let id = Identity::generate();
        let text = id.to_bech32();
        assert!(text.starts_with("AGE-SECRET-KEY-1"));
        let again = Identity::parse(&text).unwrap();
        assert_eq!(again.recipient(), id.recipient());
        assert!(!format!("{id:?}").contains("AGE-SECRET"));
        let r = Recipient::parse(&id.recipient().to_bech32()).unwrap();
        assert_eq!(r, id.recipient());
        assert!(Identity::parse_file("# created: now\n# public key: x\n").is_err());
        let e = Identity::parse("AGE-SECRET-KEY-1QQQQ")
            .unwrap_err()
            .to_string();
        assert!(!e.contains("QQQQ"), "{e}");
    }

    #[test]
    fn f_044_a_file_key_wraps_to_x25519_and_only_its_identity_unwraps_it() {
        let id = Identity::generate();
        let other = Identity::generate();
        let fk = new_file_key();
        let s = id.recipient().wrap(&fk).unwrap();
        assert_eq!(id.unwrap(&s).as_deref(), Some(&*fk));
        assert!(other.unwrap(&s).is_none());
    }

    #[test]
    fn f_044_a_header_round_trips_and_its_mac_binds_the_file_key() {
        let fk = new_file_key();
        let stanzas = vec![
            Stanza {
                kind: "aledgent-transit".into(),
                args: vec!["archive".into(), "1".into()],
                body: vec![9u8; 512],
            },
            Stanza {
                kind: "aledgent-seal".into(),
                args: vec!["ab".repeat(32)],
                body: Vec::new(),
            },
            // A body whose base64 is exactly 64 columns: an empty last line follows it.
            Stanza {
                kind: "x".into(),
                args: Vec::new(),
                body: vec![1u8; 48],
            },
        ];
        let mut file = encode_header(&fk, &stanzas).unwrap();
        file.extend_from_slice(b"PAYLOAD");
        let (h, at) = parse_header(&file).unwrap();
        assert_eq!(&file[at..], b"PAYLOAD");
        assert_eq!(h.stanzas, stanzas);
        check_mac(&h, &fk).unwrap();
        assert!(check_mac(&h, &new_file_key()).is_err());
        // A changed argument breaks the MAC.
        let mut bad = file.clone();
        let i = bad.windows(7).position(|w| w == b"archive").unwrap();
        bad[i] = b'A';
        let (h2, _) = parse_header(&bad).unwrap();
        assert!(check_mac(&h2, &fk).is_err());
    }

    #[test]
    fn f_044_the_payload_round_trips_at_chunk_edges_and_refuses_tampering() {
        let fk = new_file_key();
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK, 2 * CHUNK + 5] {
            let pt: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let ct = encrypt_payload(&fk, &pt);
            assert_eq!(decrypt_payload(&fk, &ct, len as u64).unwrap(), pt, "{len}");
            if len > 0 {
                assert!(decrypt_payload(&fk, &ct, len as u64 - 1).is_err());
                let mut t = ct.clone();
                let last = t.len() - 1;
                t[last] ^= 1;
                assert!(decrypt_payload(&fk, &t, len as u64).is_err());
            }
            // Cut at a chunk boundary: the last chunk is gone and the one before is not final.
            if len > CHUNK {
                assert!(decrypt_payload(&fk, &ct[..16 + CHUNK + TAG], len as u64).is_err());
            }
        }
    }
}
