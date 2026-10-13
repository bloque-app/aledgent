// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `stress-config`: a throwaway deployment for a stress run. It writes ingress's configuration
//! (which `aledgent-core-host serve` reads too) and a file of **test** secrets, drawn fresh for
//! each run and worth nothing anywhere: the operator's token, the core's key-ring root, ingress's
//! sealing and authentication keys, the clerk key, the Agent's keys. Never point a real
//! deployment at them (`REQ-AU-8`): they are written to a file the run deletes with its runner.
//!
//! With the core host (`--core-endpoint`), `F-035` holds even here: the core's four keys go to
//! the signer service, as files in `signer/` with the signer's configuration (`signer.json`,
//! development keys), and none to the core host's environment (`core.env`); ingress and the core
//! host share the link's token (`ALEDGENT_CORE_LINK_TOKEN` in `secrets.env`, its hash in
//! `core_link`). The in-process core still takes its root and clerk key from `secrets.env`.

use std::path::Path;

use ingress::config::{
    DeploymentConfig, ENV_AUTHN_KEY, ENV_CLERK_KEY, ENV_CORE_ROOT, ENV_DATABASE_URL,
    ENV_SEALING_KEY,
};
use serde_json::json;
use sha2::Digest as _;

use super::world::{ORIGIN, RP_ID};

/// The assets of a stress deployment: a card asset and four issued ones.
pub const ASSETS: &[(&str, u8)] = &[
    ("DUSD", 6),
    ("COPM", 2),
    ("TSTA", 2),
    ("TSTB", 2),
    ("TSTC", 6),
];

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    let _ = getrandom::fill(&mut b);
    b
}

/// What `stress-config` writes.
pub struct Options<'a> {
    /// Where.
    pub out: &'a Path,
    /// The PostgreSQL URL ingress uses.
    pub database_url: &'a str,
    /// Where ingress listens.
    pub listen: &'a str,
    /// The core host's endpoint (`unix:<path>`), for the core as a VOS Agent; none for the
    /// in-process core.
    pub core_endpoint: Option<&'a str>,
    /// The anchoring interval, ms; none for no anchoring.
    pub anchoring_ms: Option<u64>,
    /// The in-process core's journal.
    pub journal: &'a str,
}

/// Write `ingress.json`, `operator.token` and `secrets.env` into `out`.
pub fn write(o: &Options<'_>) -> Result<(), String> {
    std::fs::create_dir_all(o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    let token = format!("stress-operator-{}", hex::encode(random32()));
    let genesis_op = hex::encode(&random32()[..16]);
    let deployment = hex::encode(&random32()[..16]);
    let genesis_at = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|e| e.to_string())?;
    let assets: Vec<_> = ASSETS
        .iter()
        .map(|(id, p)| json!({ "id": id, "precision": p, "kind": "stable" }))
        .collect();
    let mut genesis = json!({
        "assets": assets,
        "operator_verifier": "0f",
        "deployment": deployment,
        "decision_bound_ms": 60_000,
        "review_service_level_ms": 3_600_000,
        "default_deadlines_ms": { "reserve": 600_000, "transfer": 600_000, "redeem": 600_000 },
        "genesis_op": genesis_op,
        "genesis_at": genesis_at,
    });
    if let Some(ms) = o.anchoring_ms {
        genesis["anchoring"] = json!({ "interval_ms": ms });
    }
    // The operator's identity and credential are known from the file before the first start.
    let mut config = json!({
        "listen": o.listen,
        "replica": "ingress-stress-0",
        // A throwaway deployment over the runner's own PostgreSQL (T-009-14).
        "profile": "development",
        "require_sync_standby": false,
        "tokens": [],
        "allow_holder_credentials": true,
        "passkeys": {
            "relying_parties": [{ "rp_id": RP_ID, "origins": [ORIGIN], "registrations": true }],
            "require_user_verification": true,
            "challenge_ttl_ms": 120_000,
            "session_ttl_ms": 900_000,
        },
        "tick_interval_ms": 1000,
        "decision_bound_ms": 60_000,
        "core_journal": o.journal,
        "genesis": genesis,
    });
    // F-035: the core host's link token and its keys behind the signer service.
    let link_token = format!("stress-link-{}", hex::encode(random32()));
    let signer_token = format!("stress-signer-{}", hex::encode(random32()));
    if let Some(e) = o.core_endpoint {
        config["core_endpoint"] = json!(e);
        config["core_link"] = json!({
            "token_sha256": [hex::encode(sha2::Sha256::digest(link_token.as_bytes()))],
        });
        config["core_keys"] = json!({
            "signer": {
                "socket": o.out.join("signer").join("signer.sock"),
                "client": "core",
                "token_file": o.out.join("signer").join("core.token"),
            },
            "key_ring": "keyring",
            "clerk": "clerk",
            "agent_authority": "agent-authority",
            "agent_package": "agent-package",
        });
    }
    let parsed: DeploymentConfig =
        serde_json::from_value(config.clone()).map_err(|e| format!("the config: {e}"))?;
    let founded = parsed.genesis.founded()?;
    config["tokens"] = json!([{
        "token_sha256": hex::encode(sha2::Sha256::digest(token.as_bytes())),
        "principal": founded.operator.to_did(),
        "credential": founded.operator_credential.to_did(),
        "kind": "service",
    }]);
    let _: DeploymentConfig =
        serde_json::from_value(config.clone()).map_err(|e| format!("the config: {e}"))?;
    let write = |name: &str, text: &str| {
        let path = o.out.join(name);
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    };
    write(
        "ingress.json",
        &serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?,
    )?;
    write("operator.token", &token)?;
    write("operator.identity", &founded.operator.to_did())?;
    // The clerk key must be a canonical scalar: below the group order, which is just above
    // 2^252, so a top byte under 0x10 is always canonical.
    let mut clerk = random32();
    clerk[31] &= 0x0f;
    let root = random32();
    let (agent, package) = (random32(), random32());
    let mut secrets = vec![
        (ENV_DATABASE_URL, o.database_url.to_owned()),
        (ENV_SEALING_KEY, hex::encode(random32())),
        (ENV_AUTHN_KEY, hex::encode(random32())),
        (ingress::config::ENV_CORE_LINK_TOKEN, link_token.clone()),
    ];
    if o.core_endpoint.is_some() {
        // F-035: the core host's keys are the signer's. Its user id is this process's (one
        // runner, one user).
        let dir = o.out.join("signer");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            Ok::<_, String>(path)
        };
        let source = |name: &str, secret: &[u8; 32]| -> Result<serde_json::Value, String> {
            Ok(json!({ "file": { "path": file(&format!("{name}.hex"), &hex::encode(secret))? } }))
        };
        file("core.token", &signer_token)?;
        #[cfg(unix)]
        let uid = {
            use std::os::unix::fs::MetadataExt as _;
            std::fs::metadata(dir.join("core.token"))
                .map_err(|e| e.to_string())?
                .uid()
        };
        #[cfg(not(unix))]
        let uid = 0u32;
        let signer = json!({
            "listen": dir.join("signer.sock"),
            "development": true,
            "clients": [{
                "name": "core",
                "uids": [uid],
                "token_sha256": [hex::encode(sha2::Sha256::digest(signer_token.as_bytes()))],
                "keys": ["keyring", "clerk", "agent-authority", "agent-package"],
            }],
            "keys": [
                { "id": "keyring", "scheme": "key_ring", "source": source("keyring", &root)? },
                { "id": "clerk", "scheme": "clerk", "source": source("clerk", &clerk)? },
                { "id": "agent-authority", "scheme": "ed25519", "source": source("agent-authority", &agent)? },
                { "id": "agent-package", "scheme": "ed25519", "source": source("agent-package", &package)? },
            ],
        });
        file(
            "signer.json",
            &serde_json::to_string_pretty(&signer).map_err(|e| e.to_string())?,
        )?;
    } else {
        // The in-process core holds its root and clerk key (development only).
        secrets.push((ENV_CORE_ROOT, hex::encode(root)));
        secrets.push((ENV_CLERK_KEY, hex::encode(clerk)));
    }
    let text: String = secrets.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    write("secrets.env", &text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for f in ["secrets.env", "operator.token"] {
            let _ = std::fs::set_permissions(o.out.join(f), std::fs::Permissions::from_mode(0o600));
        }
    }
    println!(
        "stress-config: {} (operator {}), test secrets in secrets.env",
        o.out.join("ingress.json").display(),
        founded.operator.to_did()
    );
    Ok(())
}
