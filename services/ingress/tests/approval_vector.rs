// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The vector of amendment 0037, F5, through ingress's own path (`T-040-02`, `AC-A10.9`): each of
//! the five command bodies is parsed as the command route parses it, written again in ingress's
//! canonical text, and digested with the deployment, the environment, the client key, the
//! initiator, the acted-for identity and the payload digest, exactly as step (a) and step (b) do.
//! The texts and digests must equal the amendment's byte for byte (a difference in a member name
//! fails here), and the second implementation (`approval_vector.py`, the amendment's own script)
//! must print the same values.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ingress::approval::{ApprovalMode, Approvals, digest_of};
use ingress::auth::{Authenticated, CredentialKind, Proof};
use ingress::command::Command;
use ingress::credentials::Pending;
use ingress::wire::{CommandBody, PasskeyRegistrationBody};
use ledger_core::id::IdentityId;

const ME: &str = "did:aledgent:id:0H248H248H248H248H248H248H";
const OTHER: &str = "did:aledgent:id:0J289144GJ289144GJ289144GJ";

struct Case {
    name: &'static str,
    client_key: &'static str,
    on_behalf_of: Option<&'static str>,
    text: &'static str,
    digest: &'static str,
    registration: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "1 transfer, one leg",
        client_key: "pay-2026-10-21-0001",
        on_behalf_of: None,
        text: r#"{"command":"transfer","legs":[{"amount":"2500","asset":"USD","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:248H248H248H248H248H248H24"}]}"#,
        digest: "c85b6b14cd4b575b29fbbf46ff99a8d87c65274a6f775e882f8ad2bc179570de",
        registration: false,
    },
    Case {
        name: "2 transfer, two legs, deadline, on behalf of",
        client_key: "pay-2026-10-21-0002",
        on_behalf_of: Some(OTHER),
        text: r#"{"command":"transfer","deadline":1800000600000,"legs":[{"amount":"2500","asset":"USD","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:248H248H248H248H248H248H24"},{"amount":"10","asset":"EURC","from":"did:aledgent:acct:1K6CSK6CSK6CSK6CSK6CSK6CSK","to":"did:aledgent:acct:2NANANANANANANANANANANANAN"}]}"#,
        digest: "bdb0423d7c5556735790fd26ce46885507bb30be9de7162214ee5efc55b050cd",
        registration: false,
    },
    Case {
        name: "3 capture",
        client_key: "cap-0003",
        on_behalf_of: None,
        text: r#"{"cleared":"1000","command":"capture","destination":"did:aledgent:acct:248H248H248H248H248H248H24","reservation":"did:aledgent:op:1248H248H248H248H248H248H2"}"#,
        digest: "bf20de62f3858c88cbe57a6af41d18ee7fa84ce5c7485f059be42debfe52c689",
        registration: false,
    },
    Case {
        name: "4 approve_reversal, escapes",
        client_key: "rev-0004",
        on_behalf_of: None,
        text: r#"{"command":"operator","justification":"ok \"x\" \\ \u007f \u00e9 \ud83d\ude00","op":"approve_reversal","proposal":"did:aledgent:op:36CSK6CSK6CSK6CSK6CSK6CSK6"}"#,
        digest: "95ba018a974a400dad4235a9ea3bd7285f23a20d19b1fb042e13ca831c09b8d4",
        registration: false,
    },
    Case {
        name: "5 register_credential, device-bound",
        client_key: "cred-0005",
        on_behalf_of: None,
        text: r#"{"command":"register_credential","marker":"ordinary","scheme":"webauthn-es256","verifier":""}"#,
        digest: "19c7a71988ef273bcb06529c74089aa18812dcc8e9cd5fab26870908c1552f61",
        registration: true,
    },
];

fn approvals() -> Approvals {
    Approvals::new(
        ApprovalMode::Enforce,
        Some("prod-eu"),
        "d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9",
    )
    .unwrap()
}

fn me() -> Authenticated {
    Authenticated {
        principal: ME.into(),
        credential: "did:aledgent:cred:36CSK6CSK6CSK6CSK6CSK6CSK6".into(),
        kind: CredentialKind::Holder,
        proof: Proof::Session,
    }
}

/// Ingress's text and digest for one case, by the functions the two steps use.
fn ingress_side(c: &Case) -> (String, String) {
    let body: CommandBody = serde_json::from_str(c.text).expect("the vector's body parses");
    let command = Command::parse(&body).expect("the vector's command validates");
    let pending = c.registration.then(|| {
        Pending::RegisterPasskey(PasskeyRegistrationBody {
            client_data_json: URL_SAFE_NO_PAD.encode(br#"{"type":"webauthn.create"}"#),
            attestation_object: URL_SAFE_NO_PAD.encode((0x40u8..0x60).collect::<Vec<_>>()),
        })
    });
    let obo = c.on_behalf_of.map(|o| IdentityId::parse(o).unwrap());
    let (text, digest) = digest_of(
        &approvals(),
        &me(),
        obo.as_ref(),
        c.client_key,
        pending.as_ref(),
        &command,
    )
    .expect("approvable");
    (String::from_utf8(text).unwrap(), hex::encode(digest))
}

#[test]
fn ac_a10_9_ingress_writes_the_vectors_text_and_digest_byte_for_byte() {
    for c in CASES {
        let (text, digest) = ingress_side(c);
        assert_eq!(text, c.text, "case {}: canonical text", c.name);
        assert_eq!(digest, c.digest, "case {}: command digest", c.name);
    }
}

/// `AC-A10.9`: the second implementation, run, prints what ingress computes.
#[test]
fn ac_a10_9_the_second_implementation_agrees() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/approval_vector.py");
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(&script)
        .output()
        .expect("python3 runs the second implementation");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8(out.stdout).unwrap();
    for c in CASES {
        let (text, digest) = ingress_side(c);
        let block = printed
            .split("case ")
            .find(|b| b.starts_with(c.name))
            .unwrap_or_else(|| panic!("case {} printed", c.name));
        assert!(
            block.contains(&format!("  text: {text}\n")),
            "case {}",
            c.name
        );
        assert!(
            block.contains(&format!("  command digest: {digest}\n")),
            "case {}",
            c.name
        );
    }
    for line in [
        "identity digest: 073c7ff1e2f11c45844b0a9df8966d57d7b4f4bcf5acdddb8655f35911445a57",
        "assertion digest: b3839496408b93ebc48701d29ea062231557b19969add218999e24e922a1e9b9",
        "key approval signed value (SHA-256 of it): 246aa66842214fd544fd859f6744f1ad273c5946eca4ef47bc614525e2764773",
    ] {
        assert!(printed.contains(line), "{line}");
    }
}
