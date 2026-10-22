# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The second implementation of amendment 0037's formats (T-040-02, AC-A10.9): the amendment's own
# script, standard-library Python, written without the Rust code. tests/approval_vector.rs runs it
# and compares every value with ingress's.

import base64, hashlib, hmac, json, struct

ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"          # Crockford base32, upper case

def did(kind, byte):                                   # did:aledgent:<kind>:<26 characters> from 16 equal bytes
    v = int.from_bytes(bytes([byte]) * 16, "big")
    return f"did:aledgent:{kind}:" + "".join(ALPHABET[(v >> (5 * (25 - i))) & 31] for i in range(26))

def frame(*fields):                                    # each field: 4-byte big-endian length, then the bytes
    return b"".join(struct.pack(">I", len(f)) + f for f in fields)

sha = lambda b: hashlib.sha256(b).digest()
def canon(body):                                       # the canonical text of F1
    return json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()

DEPLOYMENT = bytes([0xD9]) * 16                        # the book's identity (genesis)
ENVIRONMENT = b"prod-eu"                               # the new ingress setting: a name a restore does not copy
ME, OTHER = did("id", 0x11), did("id", 0x12)
A1, A2, A3 = did("acct", 0x33), did("acct", 0x44), did("acct", 0x55)

def command_digest(client_key, initiator, on_behalf_of, payload_digest, text):
    obo = [b"\x00"] if on_behalf_of is None else [b"\x01", on_behalf_of.encode()]
    pre = frame(b"aledgent/approval/command", b"\x01", DEPLOYMENT, ENVIRONMENT, client_key.encode(),
                initiator.encode(), *obo, payload_digest, text)
    return pre, sha(pre)

cases = {}
# 1. one-leg transfer, no deadline, acting for nobody else
cases["1 transfer, one leg"] = (b"pay-2026-10-21-0001", ME, None, b"", canon(
    {"command": "transfer", "legs": [{"from": A1, "to": A2, "asset": "USD", "amount": "2500"}]}))
# 2. two legs, a deadline, acting for another identity
cases["2 transfer, two legs, deadline, on behalf of"] = (b"pay-2026-10-21-0002", ME, OTHER, b"", canon(
    {"command": "transfer", "deadline": 1800000600000, "legs": [
        {"from": A1, "to": A2, "asset": "USD", "amount": "2500"},
        {"from": A1, "to": A3, "asset": "EURC", "amount": "10"}]}))
# 3. capture: the reservation is in the command text
cases["3 capture"] = (b"cap-0003", ME, None, b"", canon(
    {"command": "capture", "reservation": did("op", 0x22), "destination": A2, "cleared": "1000"}))
# 4. an escaped string: quote, backslash, DEL, e-acute, and a character above U+FFFF
just = 'ok "x" \\ \x7f é \U0001F600'
cases["4 approve_reversal, escapes"] = (b"rev-0004", ME, None, b"", canon(
    {"command": "operator", "op": "approve_reversal", "proposal": did("op", 0x66), "justification": just}))
# 5. adding a device-bound credential: the registration payload is bound by its digest
client_data, attestation = b'{"type":"webauthn.create"}', bytes(range(0x40, 0x60))
payload = sha(frame(b"aledgent/approval/payload", b"\x01", client_data, attestation))
cases["5 register_credential, device-bound"] = (b"cred-0005", ME, None, payload, canon(
    {"command": "register_credential", "marker": "ordinary", "scheme": "webauthn-es256", "verifier": ""}))

out = []
digests = {}
for name, (ck, who, obo, pay, text) in cases.items():
    pre, d = command_digest(ck.decode(), who, obo, pay, text)
    digests[name] = d
    out += [f"case {name}", f"  text: {text.decode()}", f"  preimage bytes: {len(pre)}", f"  command digest: {d.hex()}"]
out.append(f"  (case 5) payload digest: {payload.hex()}")

# The approval challenge, version 1, for case 1 (the seal covers the deployment too).
d1 = digests["1 transfer, one leg"]
authn_key, nonce, expires_at = bytes(range(32)), bytes([0x55]) * 16, 1_800_000_120_000      # milliseconds
identity_digest = sha(ME.encode())
exp = struct.pack(">Q", expires_at)
m = hmac.new(authn_key, b"aledgent/approval-challenge/v1", hashlib.sha256)
for part in (nonce, exp, b"\x03", identity_digest, d1, DEPLOYMENT, ENVIRONMENT):
    m.update(struct.pack(">Q", len(part)) + part)       # the seal frames with 8-byte lengths
challenge = nonce + exp + b"\x03" + identity_digest + d1 + m.digest()
out += [f"identity digest: {identity_digest.hex()}", f"challenge (121 bytes): {challenge.hex()}"]

# The assertion digest and the record's signed message (stand-in assertion bytes).
raw_id    = bytes(range(0x80, 0x90))
auth_data = sha(b"example.com") + b"\x05" + struct.pack(">I", 7)       # rpIdHash, flags UP+UV, count 7
b64 = base64.urlsafe_b64encode(challenge).rstrip(b"=").decode()
client_data_json = ('{"type":"webauthn.get","challenge":"%s","origin":"https://auth.example.com"}' % b64).encode()
signature = bytes([0xA5]) * 70
assertion_digest = sha(frame(b"aledgent/approval/assertion", b"\x01", raw_id, auth_data, client_data_json, signature))
credential = bytes([0x66]) * 16
record_message = frame(b"aledgent/approval/record", b"\x01", credential, d1, assertion_digest, struct.pack(">I", 1))
out += [f"assertion digest: {assertion_digest.hex()}", f"record signed message: {record_message.hex()}"]

# A `key` credential's approval: the key signs the 32-byte SHA-256 of this framed message.
key_message = frame(b"aledgent/approval/key", b"\x01", d1, struct.pack(">Q", expires_at))
out.append(f"key approval framed message: {key_message.hex()}")
out.append(f"key approval signed value (SHA-256 of it): {sha(key_message).hex()}")
print("\n".join(out))
