// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # aledgent as a CI service container
//!
//! One process that starts ready to serve: ingress (`F-009`) over its in-memory store, and the
//! in-process core behind it, **book only**: no settlement adapter, no chain, no PostgreSQL, no
//! Vault, no cloud credentials. It is for another repository's CI to run next to its own tests
//! (a GitHub Actions `services:` container), as `deploy/docker/SIDECAR.md` describes.
//!
//! **Test only.** Everything it needs is a constant of this crate: the core's key-ring root, the
//! genesis, and the operator's bearer token, [`TEST_OPERATOR_TOKEN`]. They are public, worth
//! nothing, and exist so that a test suite can call the sidecar without being handed a secret.
//! That is exactly what `REQ-AU-8` forbids a deployment to do (no credential has a default), so
//! none of it is reachable from the production binaries: this crate is its own binary
//! (`aledgent-sidecar`) in its own image (`aledgent-sidecar`), and `aledgent-ingress` is unchanged.
//! The book lives in memory and is gone when the process ends.
//!
//! What it seeds before it listens, so that "it answers" means "it is ready":
//!
//! - the asset registry, [`DEFAULT_ASSETS`] unless `ALEDGENT_SIDECAR_ASSETS` names others;
//! - the book's `max_legs` (`ALEDGENT_SIDECAR_MAX_LEGS`, the kernel's default of 10 unless set):
//!   a transfer of more legs is refused `ERR-BatchTooLarge` (HTTP 413) before anything is looked
//!   up;
//! - the operator (genesis founds it, `REQ-AS-2`), recognised under [`TEST_OPERATOR_TOKEN`] and
//!   under every token of `ALEDGENT_SIDECAR_EXTRA_TOKENS`, all as the same service principal and
//!   credential;
//! - one issuance grant per asset, held by the operator, so that it can issue into any account
//!   (`REQ-AU-10`);
//! - one operator grant over every account in delegated custody, held by the operator
//!   ([`OPERATOR_OPS`]), so that it reaches the accounts it opens for its customers: reads them
//!   (`INV-16`) and moves value between them, as an integrating platform does.
//!
//! - the notification recipients of `ALEDGENT_SIDECAR_RECIPIENTS` ([`parse_recipients`]), if
//!   any: each an endpoint and the **name** of the variable holding its secret, registered as
//!   `aledgent-ingress recipient` registers one ([`ingress::store::Recipient::with_secret_env`]).
//!   None by default, so notifications are parked, as before.
//! - the **service principals** of a configuration ([`parse_principals`], [`load_principals`]):
//!   `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS` (inline JSON) or
//!   `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE` (a path), else the sample [`DEFAULT_PRINCIPALS`].
//!   Each is **its own** principal (an organisation identity the operator opens) under a name the
//!   configuration gives it, recognised under its own token (read from a named variable, or, when
//!   the configuration opts in, its public test token; never the operator's), with the grants of
//!   its role ([`PrincipalRole`]: issuance authority, operator authority, or the identities
//!   principal; `REQ-AU-7`, `REQ-ID-6`, amendment 0025);
//! - the **system accounts** of `ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS` ([`parse_system_accounts`]),
//!   if any: each opened under the operator's identity, named by its role alias and granted,
//!   account-scoped, to the service principal that operates it (`ops/runbooks/system-accounts.md`,
//!   ruling `au4-q2`).
//!
//! - **passkeys** of one relying party (`F-028`): `ALEDGENT_SIDECAR_RP_ID` (default
//!   `localhost`) with the origins of `ALEDGENT_SIDECAR_RP_ORIGINS` (default `http://localhost`),
//!   challenges and sessions MAC'd under [`TEST_AUTHN_KEY`], so a CI job can run a passkey's
//!   registration and sign-in end to end.
//!
//! Withdrawals have nowhere to go: no settlement adapter serves, so a withdrawal is refused or
//! waits as the core decides without one. Disclosures are refused (no clerk key).

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use authn::token::AuthnKey;
use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use ingress::auth::{Authenticator, CredentialKind, TokenEntry};
use ingress::boundary::{
    CallerBody, Core as _, CoreInput, CoreReply, InputBody, InputKey, Outcome,
};
use ingress::clock::{SharedClock, SystemClock};
use ingress::config::{AssetConfig, GenesisConfig, PasskeysConfig, RelyingPartyConfig};
use ingress::delivery::HttpTransport;
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::service::{Config, Replica};
use ingress::store::mem::MemStore;
use ingress::store::{Recipient, Store as _};
use ingress::surface::router;
use ingress::testkit::{TestCore, call};
use ingress::wire::{CommandBody, IdentityKindBody, OpenIdentityBody};
use ledger_core::id::{CredentialId, IdentityId, OperationId};
use ledger_core::ledger::Founded;
use ledger_core::lifecycle::keys::{ClientKey, Surface, operation_reference};
use ledger_core::party::KeyDomain;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The operator's bearer token. **Test only**: public, documented, worth nothing anywhere.
pub const TEST_OPERATOR_TOKEN: &str = "aledgent-sidecar-test-operator-token";

/// The key that MACs passkey challenges and sessions (`F-028`). **Test only.**
pub const TEST_AUTHN_KEY: [u8; 32] = [0xA7; 32];

/// The passkeys' relying party when `ALEDGENT_SIDECAR_RP_ID` is not set.
pub const DEFAULT_RP_ID: &str = "localhost";

/// The relying party's origins when `ALEDGENT_SIDECAR_RP_ORIGINS` is not set.
pub const DEFAULT_RP_ORIGINS: &str = "http://localhost";

/// The environment variable naming the passkeys' relying party, its `rpId`.
pub const ENV_RP_ID: &str = "ALEDGENT_SIDECAR_RP_ID";

/// The environment variable naming the relying party's origins, comma separated, matched exactly:
/// `https://…`, or `http://localhost[:port]`.
pub const ENV_RP_ORIGINS: &str = "ALEDGENT_SIDECAR_RP_ORIGINS";

/// The root of the in-process core's key ring. **Test only.**
pub const TEST_CORE_ROOT: [u8; 32] = [0x5C; 32];

/// Genesis's operation reference. Every founded identifier (the operator, its credential, the
/// issuance account) is minted from it alone, so they are the same on every start.
pub const GENESIS_OP: [u8; 16] = [0x5E; 16];

/// The deployment's identity.
pub const DEPLOYMENT: [u8; 16] = [0x5D; 16];

/// The registry seeded when `ALEDGENT_SIDECAR_ASSETS` is not set: `SYMBOL/precision`, comma
/// separated.
pub const DEFAULT_ASSETS: &str = "COPM/2,TSTA/2,TSTB/2,TSTC/6";

/// Where it listens when `ALEDGENT_SIDECAR_LISTEN` is not set.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

/// The environment variable naming the address to listen on, `host:port`.
pub const ENV_LISTEN: &str = "ALEDGENT_SIDECAR_LISTEN";

/// The environment variable naming the registry, as [`DEFAULT_ASSETS`].
pub const ENV_ASSETS: &str = "ALEDGENT_SIDECAR_ASSETS";

/// The environment variable naming extra bearer tokens, comma separated. Each is recognised as the
/// operator, the same principal and credential as [`TEST_OPERATOR_TOKEN`], which stays. Their
/// values are never logged or served: only their number is ([`Sidecar::describe`]).
pub const ENV_EXTRA_TOKENS: &str = "ALEDGENT_SIDECAR_EXTRA_TOKENS";

/// The environment variable naming the book's `max_legs` (the kernel's
/// `LifecycleConfig::max_legs`): the most legs one transfer submission may carry. Unset, the
/// kernel's default ([`ledger_core::lifecycle::DEFAULT_MAX_LEGS`], 10).
pub const ENV_MAX_LEGS: &str = "ALEDGENT_SIDECAR_MAX_LEGS";

/// The environment variable naming the notification recipients to register at start, as
/// [`parse_recipients`] reads them. Each names the variable holding its secret, never the secret.
pub const ENV_RECIPIENTS: &str = "ALEDGENT_SIDECAR_RECIPIENTS";

/// The environment variable holding the service principals' configuration, inline JSON, as
/// [`parse_principals`] reads it. Each principal names the variable holding its token, never the
/// token. Unset (and [`ENV_SERVICE_PRINCIPALS_FILE`] unset), [`DEFAULT_PRINCIPALS`].
pub const ENV_SERVICE_PRINCIPALS: &str = "ALEDGENT_SIDECAR_SERVICE_PRINCIPALS";

/// The environment variable naming a file that holds the service principals' configuration, as
/// [`ENV_SERVICE_PRINCIPALS`] would inline it. Setting both fails the start.
pub const ENV_SERVICE_PRINCIPALS_FILE: &str = "ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE";

/// The environment variable naming the system accounts to open at start, as
/// [`parse_system_accounts`] reads them.
pub const ENV_SYSTEM_ACCOUNTS: &str = "ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS";

/// What a service principal's account-scoped grant on a system account it operates lets it do
/// (`ops/runbooks/system-accounts.md` step 1, ruling `au4-q2`).
pub const SYSTEM_ACCOUNT_OPS: &[&str] = &["transfer", "reserve", "capture", "release"];

/// The alias source of a system account's role (`ops/runbooks/system-accounts.md` step 3).
pub const SYSTEM_ROLE_SOURCE: &str = "system-role";

/// The default deadline of a reservation, a transfer and a redemption: one hour, longer than any
/// CI job waits on one.
pub const DEFAULT_DEADLINE_MS: u64 = 3_600_000;

/// The bound of every seeded grant (`REQ-AU-2`: none is unbounded): what it may move per day, in
/// minor units. Far above anything a test moves.
pub const PER_DAY_LIMIT: &str = "1000000000000000";

/// What the operator's grant over every account in delegated custody lets it do: what an
/// integrating platform that holds its customers' accounts does with them, reads included.
pub const OPERATOR_OPS: &[&str] = &[
    "transfer",
    "reserve",
    "capture",
    "release",
    "convert",
    "encumber",
    "disencumber",
    "close_account",
];

/// The registry from its spelling, `SYMBOL/precision[,…]`. Every asset is registered `stable`.
pub fn parse_assets(spec: &str) -> Result<Vec<AssetConfig>, String> {
    let mut out: Vec<AssetConfig> = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (id, precision) = item
            .split_once('/')
            .ok_or_else(|| format!("{item}: SYMBOL/precision"))?;
        let precision: u8 = precision
            .parse()
            .map_err(|_| format!("{item}: the precision is not a small integer"))?;
        let id = id.trim().to_ascii_uppercase();
        if out.iter().any(|a| a.id == id) {
            return Err(format!("{id} is listed twice"));
        }
        out.push(AssetConfig {
            id,
            precision,
            kind: "stable".into(),
            deprecated: false,
        });
    }
    if out.is_empty() {
        return Err("no asset".into());
    }
    Ok(out)
}

/// The extra bearer tokens from their spelling, comma separated: trimmed, empty items dropped, a
/// repeat (or [`TEST_OPERATOR_TOKEN`] again) kept once. An error never repeats a value.
pub fn parse_tokens(spec: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for (i, item) in spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        if item.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("token {}: contains whitespace", i + 1));
        }
        if item != TEST_OPERATOR_TOKEN && !out.iter().any(|t| t == item) {
            out.push(item.to_owned());
        }
    }
    Ok(out)
}

/// One notification recipient to register at start ([`ENV_RECIPIENTS`]): whose notifications,
/// where they go and the name of the variable holding the secret they are signed with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipientSpec {
    /// The principal, a `did:` or a service principal's name ([`PrincipalSpec::name`]), resolved
    /// to that service principal at start; `None` is the operator (written `operator`, or
    /// nothing).
    pub principal: Option<String>,
    /// Where its notifications are pushed, `http://` or `https://`.
    pub endpoint: String,
    /// The name of the environment variable whose value (its raw UTF-8 bytes) is the secret.
    pub secret_env: String,
}

/// The recipients from their spelling, `;` separated, each `[PRINCIPAL=]ENDPOINT#SECRET_VAR`:
///
/// - `PRINCIPAL` is `operator`, a service principal's name (that principal, which the
///   configuration of `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS` must name) or a `did:`; left out, the
///   operator;
/// - `ENDPOINT` is an `http://` or `https://` URL;
/// - `SECRET_VAR` is the **name** of the environment variable holding the secret
///   (`[A-Za-z_][A-Za-z0-9_]*`), split at the last `#`.
///
/// Empty items are dropped. A principal named twice is an error: the store keeps one recipient per
/// principal. An error never repeats a value read from the environment.
pub fn parse_recipients(spec: &str) -> Result<Vec<RecipientSpec>, String> {
    let mut out: Vec<RecipientSpec> = Vec::new();
    for (i, item) in spec
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let n = i + 1;
        // A principal is before the first `=`, when that part holds no `/` (a URL's query may
        // hold `=` too, but always after `://`).
        let (principal, rest) = match item.split_once('=') {
            Some((p, rest)) if !p.contains('/') => {
                let p = p.trim();
                let principal = if p == "operator" {
                    None
                } else if is_principal_name(p)
                    || (p.starts_with("did:") && !p.chars().any(char::is_whitespace))
                {
                    Some(p.to_owned())
                } else {
                    return Err(format!(
                        "recipient {n}: {p:?} is neither operator, a service principal's name \
                         nor a did:"
                    ));
                };
                (principal, rest.trim())
            }
            _ => (None, item),
        };
        let (endpoint, var) = rest
            .rsplit_once('#')
            .ok_or_else(|| format!("recipient {n}: ENDPOINT#SECRET_VAR"))?;
        let endpoint = endpoint.trim();
        let var = var.trim();
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
            || endpoint.chars().any(char::is_whitespace)
        {
            return Err(format!("recipient {n}: the endpoint is not an http(s) URL"));
        }
        let mut chars = var.chars();
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!(
                "recipient {n}: {var:?} is not a variable name (the secret's name, not its value)"
            ));
        }
        if out.iter().any(|r| r.principal == principal) {
            return Err(format!(
                "recipient {n}: {} is named twice",
                principal.as_deref().unwrap_or("operator")
            ));
        }
        out.push(RecipientSpec {
            principal,
            endpoint: endpoint.to_owned(),
            secret_env: var.to_owned(),
        });
    }
    Ok(out)
}

/// The role a service principal plays: which authority the sidecar grants it at start. The names
/// are `SPEC.md`'s own: the authority kinds of `REQ-AU-7` and the identities principal of
/// `REQ-ID-6`. A deployment names its principals as it likes ([`PrincipalSpec::name`]); the role
/// alone decides the grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrincipalRole {
    /// **Issuance authority** (`REQ-AS-6`, `REQ-AU-10`): one issuance grant per asset (of the
    /// registry, or of [`PrincipalSpec::assets`]), `issue` and `redeem` unless
    /// [`PrincipalSpec::ops`] narrows them. Its per-account grants on customer accounts are made
    /// by whoever opens each account, not at start.
    IssuanceAuthority,
    /// **Operator authority** over every account in delegated custody, by kind (`REQ-AU-5`):
    /// [`CUSTODY_OPS`] unless [`PrincipalSpec::ops`] names others of [`OPERATOR_OPS`]. It opens a
    /// customer's further accounts under a delegated grant over the customer's identity, which
    /// the identities principal makes (amendment 0025).
    OperatorAuthority,
    /// **The identities principal** (`REQ-ID-6`, amendment 0025): the operator-surface grant
    /// `open_identity`. It opens identities, and accounts for any identity. One holder at a time,
    /// so a configuration names at most one.
    IdentitiesPrincipal,
}

impl PrincipalRole {
    /// Every role, in the order the documentation lists them.
    pub const ALL: [PrincipalRole; 3] = [
        PrincipalRole::IssuanceAuthority,
        PrincipalRole::OperatorAuthority,
        PrincipalRole::IdentitiesPrincipal,
    ];

    /// Its name, as a configuration and `/sidecar` spell it.
    pub const fn name(self) -> &'static str {
        match self {
            PrincipalRole::IssuanceAuthority => "issuance_authority",
            PrincipalRole::OperatorAuthority => "operator_authority",
            PrincipalRole::IdentitiesPrincipal => "identities_principal",
        }
    }

    /// The role a name spells.
    pub fn parse(s: &str) -> Option<PrincipalRole> {
        PrincipalRole::ALL.into_iter().find(|r| r.name() == s)
    }
}

/// What an issuance-authority principal's grants let it do unless its configuration narrows them.
pub const ISSUANCE_OPS: &[&str] = &["issue", "redeem"];

/// What an operator-authority principal's grant over every account in delegated custody lets it
/// do unless its configuration says otherwise: by kind, transfer, reserve, capture and release
/// (`REQ-AU-5`). A batch is a transfer of several legs.
pub const CUSTODY_OPS: &[&str] = &["transfer", "reserve", "capture", "release"];

/// The sample configuration, `e2e/sidecar/principals.json`: one principal of each role under a
/// neutral name (`issuer`, `custodian`, `onboarding`), each opting in to its public test token
/// ([`test_principal_token`]). The binary opens it when neither [`ENV_SERVICE_PRINCIPALS`]
/// nor [`ENV_SERVICE_PRINCIPALS_FILE`] is set.
pub const DEFAULT_PRINCIPALS: &str = include_str!("../principals.json");

/// One service principal to open at start: a name of the deployment's choosing, its role, where
/// its token comes from and the parameters of its grants. [`check_principals`] states the rules;
/// [`parse_principals`] and every start apply them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalSpec {
    /// Its name: the key of `/sidecar`'s `service_principals`, and how a recipient
    /// ([`ENV_RECIPIENTS`]) and a system account ([`ENV_SYSTEM_ACCOUNTS`]) name it. Lower case,
    /// `[a-z][a-z0-9_-]*`, at most 64 bytes, never `operator`.
    pub name: String,
    /// Its role, which decides its grants.
    pub role: PrincipalRole,
    /// The name of the environment variable whose value is its bearer token. Exactly one of this
    /// and [`PrincipalSpec::test_token`].
    pub token_env: Option<String>,
    /// Whether it is recognised under the public test token of its name,
    /// [`test_principal_token`]. An explicit opt-in, never a fallback (`REQ-AU-8`): a principal
    /// that names neither this nor [`PrincipalSpec::token_env`] fails the start.
    pub test_token: bool,
    /// The kinds its grants name: for [`PrincipalRole::IssuanceAuthority`], of [`ISSUANCE_OPS`];
    /// for [`PrincipalRole::OperatorAuthority`], of [`OPERATOR_OPS`]; not empty, none twice. Empty
    /// for the identities principal, whose grant is `open_identity` alone.
    pub ops: Vec<String>,
    /// For [`PrincipalRole::IssuanceAuthority`] only: the assets it holds issuance authority
    /// over, each in the registry. `None` is every asset of the registry.
    pub assets: Option<Vec<String>>,
    /// The per-day bound of each grant made to it, in minor units (`REQ-AU-2`: none is
    /// unbounded): a canonical decimal from 1 to `u64::MAX`, the core's bound; [`PER_DAY_LIMIT`]
    /// unless set. Not for the identities principal.
    pub per_day_limit: String,
}

/// The public test token of the principal named `name`, when its configuration asks for one
/// (`"test_token": true`): `aledgent-sidecar-test-<name>-token`. **Test only**, like
/// [`TEST_OPERATOR_TOKEN`].
pub fn test_principal_token(name: &str) -> String {
    format!("aledgent-sidecar-test-{name}-token")
}

/// Whether `var` is a variable's name, `[A-Za-z_][A-Za-z0-9_]*`.
fn is_var_name(var: &str) -> bool {
    let mut chars = var.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `name` can name a service principal: `[a-z][a-z0-9_-]*`, at most 64 bytes, not
/// `operator` (the recipients' name for the operator).
pub fn is_principal_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && name != "operator"
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Whether `limit` is a per-day bound the core takes: a canonical decimal (no sign, no leading
/// zero) from 1 to `u64::MAX`.
fn is_per_day_limit(limit: &str) -> bool {
    limit.bytes().all(|b| b.is_ascii_digit())
        && !limit.starts_with('0')
        && limit.parse::<u64>().is_ok_and(|n| n >= 1)
}

/// The fields a principal of `role` takes.
fn fields_of(role: PrincipalRole) -> &'static [&'static str] {
    match role {
        PrincipalRole::IssuanceAuthority => &[
            "name",
            "role",
            "token_env",
            "test_token",
            "ops",
            "assets",
            "per_day_limit",
        ],
        PrincipalRole::OperatorAuthority => &[
            "name",
            "role",
            "token_env",
            "test_token",
            "ops",
            "per_day_limit",
        ],
        PrincipalRole::IdentitiesPrincipal => &["name", "role", "token_env", "test_token"],
    }
}

/// The kinds a principal of `role` may name, and its default.
fn ops_of(role: PrincipalRole) -> (&'static [&'static str], &'static [&'static str]) {
    match role {
        PrincipalRole::IssuanceAuthority => (ISSUANCE_OPS, ISSUANCE_OPS),
        PrincipalRole::OperatorAuthority => (OPERATOR_OPS, CUSTODY_OPS),
        PrincipalRole::IdentitiesPrincipal => (&[], &[]),
    }
}

/// The rules every set of service principals obeys, whether parsed ([`parse_principals`]) or
/// built by hand ([`Options::services`]): a name each, once; exactly one source of token; `ops`
/// of the role's kinds, not empty, none twice (none for the identities principal); `assets` for
/// issuance authority only, not empty, none twice; a `per_day_limit` the core takes; at most one
/// identities principal (amendment 0025). Each error names the principal and the field, and
/// never repeats a token or a `token_env` value.
pub fn check_principals(principals: &[PrincipalSpec]) -> Result<(), String> {
    for (i, p) in principals.iter().enumerate() {
        let n = i + 1;
        if !is_principal_name(&p.name) {
            return Err(format!(
                "principal {n}: {:?} is not a name ([a-z][a-z0-9_-]*, at most 64 bytes, not \
                 operator)",
                p.name
            ));
        }
        let who = format!("principal {n} ({})", p.name);
        let role = p.role.name();
        match (&p.token_env, p.test_token) {
            (Some(var), false) if is_var_name(var) => {}
            (Some(_), false) => {
                return Err(format!(
                    "{who}: token_env names no variable (the token's name, not its value)"
                ));
            }
            (Some(_), true) => {
                return Err(format!(
                    "{who}: token_env and test_token both given; name one source of its token"
                ));
            }
            (None, false) => {
                return Err(format!(
                    "{who}: no token: name its variable (token_env) or ask for its public test \
                     token (\"test_token\": true); none is assumed (REQ-AU-8)"
                ));
            }
            (None, true) => {}
        }
        let (valid, _) = ops_of(p.role);
        if valid.is_empty() {
            if !p.ops.is_empty() {
                return Err(format!(
                    "{who}: ops is not a field of the {role} role, whose grant is open_identity"
                ));
            }
        } else {
            if p.ops.is_empty() {
                return Err(format!("{who}: ops is empty"));
            }
            for (j, o) in p.ops.iter().enumerate() {
                if !valid.contains(&o.as_str()) {
                    return Err(format!(
                        "{who}: ops: {o:?} is not one of {}",
                        valid.join(", ")
                    ));
                }
                if p.ops[..j].contains(o) {
                    return Err(format!("{who}: ops names {o} twice"));
                }
            }
        }
        if let Some(assets) = &p.assets {
            if p.role != PrincipalRole::IssuanceAuthority {
                return Err(format!("{who}: assets is not a field of the {role} role"));
            }
            if assets.is_empty() {
                return Err(format!("{who}: assets is empty"));
            }
            for (j, a) in assets.iter().enumerate() {
                if assets[..j].contains(a) {
                    return Err(format!("{who}: assets names {a} twice"));
                }
            }
        }
        if p.role != PrincipalRole::IdentitiesPrincipal && !is_per_day_limit(&p.per_day_limit) {
            return Err(format!(
                "{who}: per_day_limit is not a decimal string of minor units from 1 to \
                 18446744073709551615, with no leading zero (REQ-AU-2: no grant is unbounded)"
            ));
        }
        if principals[..i].iter().any(|o| o.name == p.name) {
            return Err(format!("{who}: {} is named twice", p.name));
        }
        if p.role == PrincipalRole::IdentitiesPrincipal
            && let Some(o) = principals[..i].iter().find(|o| o.role == p.role)
        {
            return Err(format!(
                "{who}: {} already plays the identities principal, which has one holder at a \
                 time (REQ-ID-6, amendment 0025)",
                o.name
            ));
        }
    }
    Ok(())
}

/// A JSON value walked only to refuse an object that names one field twice, which `serde_json`
/// would otherwise read as its last value.
struct NoRepeatedFields;

impl<'de> serde::Deserialize<'de> for NoRepeatedFields {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(NoRepeatedFields)
    }
}

impl<'de> serde::de::Visitor<'de> for NoRepeatedFields {
    type Value = NoRepeatedFields;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_unit<E>(self) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Self, A::Error> {
        while a.next_element::<NoRepeatedFields>()?.is_some() {}
        Ok(self)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Self, A::Error> {
        let mut seen: Vec<String> = Vec::new();
        while let Some(k) = m.next_key::<String>()? {
            if seen.contains(&k) {
                return Err(serde::de::Error::custom(format!(
                    "the field {k:?} is given twice in one object"
                )));
            }
            m.next_value::<NoRepeatedFields>()?;
            seen.push(k);
        }
        Ok(self)
    }
}

/// The service principals from their configuration, JSON:
///
/// ```json
/// { "principals": [
///     { "name": "issuer", "role": "issuance_authority", "test_token": true },
///     { "name": "custodian", "role": "operator_authority", "token_env": "CUSTODIAN_TOKEN",
///       "ops": ["transfer", "reserve", "capture", "release"] },
///     { "name": "onboarding", "role": "identities_principal", "token_env": "ONBOARDING_TOKEN" }
/// ] }
/// ```
///
/// Each principal takes `name` and `role` ([`PrincipalRole::name`]); `token_env` (the **name** of
/// the variable holding its token, never the token) or `"test_token": true`, exactly one; and, by
/// role, `ops`, `assets` and `per_day_limit` (a decimal string), as [`PrincipalSpec`] says, then
/// checked by [`check_principals`]. A field it does not know, a field its role does not take, a
/// field given twice, or a value of the wrong type is an error naming the principal and the
/// field. `{"principals": []}` opens none. No error repeats a token or a `token_env` value.
pub fn parse_principals(config: &str) -> Result<Vec<PrincipalSpec>, String> {
    let config = config.trim();
    if !config.starts_with('{') {
        return Err(
            "not a JSON object, {\"principals\": [...]} (deploy/docker/SIDECAR.md); the \
             NAME=TOKEN_VAR form is retired"
                .into(),
        );
    }
    serde_json::from_str::<NoRepeatedFields>(config).map_err(|e| format!("not valid JSON: {e}"))?;
    let v: Value = serde_json::from_str(config).map_err(|e| format!("not valid JSON: {e}"))?;
    let top = v.as_object().ok_or("not a JSON object")?;
    if let Some(k) = top.keys().find(|k| k.as_str() != "principals") {
        return Err(format!("unknown field {k:?} (only \"principals\")"));
    }
    let list = top
        .get("principals")
        .ok_or("no \"principals\" list")?
        .as_array()
        .ok_or("\"principals\" is not a list")?;
    let mut out: Vec<PrincipalSpec> = Vec::new();
    for (i, p) in list.iter().enumerate() {
        let n = i + 1;
        let p = p
            .as_object()
            .ok_or_else(|| format!("principal {n}: not an object"))?;
        let name = match p.get("name") {
            None => return Err(format!("principal {n}: no \"name\"")),
            Some(v) => v
                .as_str()
                .ok_or_else(|| format!("principal {n}: name is not a string"))?,
        };
        let who = format!("principal {n} ({name})");
        let role = match p.get("role") {
            None => return Err(format!("{who}: no \"role\"")),
            Some(v) => v
                .as_str()
                .ok_or_else(|| format!("{who}: role is not a string"))?,
        };
        let role = PrincipalRole::parse(role).ok_or_else(|| {
            format!(
                "{who}: {role:?} is not a role (one of {})",
                PrincipalRole::ALL.map(PrincipalRole::name).join(", ")
            )
        })?;
        let allowed = fields_of(role);
        if let Some(k) = p.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!(
                "{who}: {k:?} is not a field of the {} role (fields: {})",
                role.name(),
                allowed.join(", ")
            ));
        }
        let token_env = match p.get("token_env") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .map(|s| s.trim().to_owned())
                    .ok_or_else(|| format!("{who}: token_env is not a string"))?,
            ),
        };
        let test_token = match p.get("test_token") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| format!("{who}: test_token is not true or false"))?,
        };
        let strings = |field: &str| -> Result<Option<Vec<String>>, String> {
            let Some(v) = p.get(field) else {
                return Ok(None);
            };
            v.as_array()
                .ok_or_else(|| format!("{who}: {field} is not a list of strings"))?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| format!("{who}: {field} holds something other than a name"))
                })
                .collect::<Result<Vec<String>, String>>()
                .map(Some)
        };
        let ops = strings("ops")?
            .unwrap_or_else(|| ops_of(role).1.iter().map(|s| (*s).to_owned()).collect());
        let assets =
            strings("assets")?.map(|a| a.into_iter().map(|a| a.to_ascii_uppercase()).collect());
        let per_day_limit = match p.get("per_day_limit") {
            None => PER_DAY_LIMIT.to_owned(),
            Some(v) => v
                .as_str()
                .ok_or_else(|| format!("{who}: per_day_limit is not a decimal string"))?
                .to_owned(),
        };
        out.push(PrincipalSpec {
            name: name.to_owned(),
            role,
            token_env,
            test_token,
            ops,
            assets,
            per_day_limit,
        });
    }
    check_principals(&out)?;
    Ok(out)
}

/// The service principals a start opens, from the inline configuration ([`ENV_SERVICE_PRINCIPALS`])
/// or the file it is in ([`ENV_SERVICE_PRINCIPALS_FILE`]), each `None` when unset: one of them,
/// parsed by [`parse_principals`]; both, an error; neither, [`DEFAULT_PRINCIPALS`]. A variable
/// that is set but blank is an error, never read as unset (an unresolved CI expansion would
/// otherwise open the sample).
pub fn load_principals(
    inline: Option<&str>,
    file: Option<&str>,
) -> Result<Vec<PrincipalSpec>, String> {
    for (var, v) in [
        (ENV_SERVICE_PRINCIPALS, inline),
        (ENV_SERVICE_PRINCIPALS_FILE, file),
    ] {
        if v.is_some_and(|v| v.trim().is_empty()) {
            return Err(format!(
                "{var} is set but blank: unset it to open the sample configuration, or pass \
                 {{\"principals\": []}} to open none"
            ));
        }
    }
    match (inline, file.map(str::trim)) {
        (Some(_), Some(_)) => Err(format!(
            "{ENV_SERVICE_PRINCIPALS} and {ENV_SERVICE_PRINCIPALS_FILE} are both set; set one"
        )),
        (Some(c), None) => {
            parse_principals(c).map_err(|e| format!("{ENV_SERVICE_PRINCIPALS}: {e}"))
        }
        (None, Some(path)) => {
            let c = std::fs::read_to_string(path)
                .map_err(|e| format!("{ENV_SERVICE_PRINCIPALS_FILE}: {path}: {e}"))?;
            parse_principals(&c).map_err(|e| format!("{ENV_SERVICE_PRINCIPALS_FILE}: {path}: {e}"))
        }
        (None, None) => parse_principals(DEFAULT_PRINCIPALS)
            .map_err(|e| format!("the sample configuration: {e}")),
    }
}

/// One system account to open at start ([`ENV_SYSTEM_ACCOUNTS`]): its role alias and the service
/// principal that operates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemAccountSpec {
    /// Its role, `<owner>/<purpose>` (`ops/runbooks/system-accounts.md` step 1).
    pub name: String,
    /// The name of the service principal granted it.
    pub service: String,
}

/// The system accounts from their spelling, `;` separated, each `ROLE_NAME=PRINCIPAL`:
///
/// - `ROLE_NAME` is the account's role, lower case, `/` separated, at most 256 bytes, of
///   `a-z 0-9 / - _ .` (`issuer/fee`); each at most once;
/// - `PRINCIPAL` is the name of a service principal of the configuration
///   ([`ENV_SERVICE_PRINCIPALS`]), checked at start.
pub fn parse_system_accounts(spec: &str) -> Result<Vec<SystemAccountSpec>, String> {
    let mut out: Vec<SystemAccountSpec> = Vec::new();
    for (i, item) in spec
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let n = i + 1;
        let (name, service) = item
            .split_once('=')
            .ok_or_else(|| format!("system account {n}: ROLE_NAME=PRINCIPAL"))?;
        let name = name.trim();
        let ok = !name.is_empty()
            && name.len() <= 256
            && !name.starts_with('/')
            && !name.ends_with('/')
            && name.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '-' | '_' | '.')
            });
        if !ok {
            return Err(format!(
                "system account {n}: {name:?} is not a role (lower case, `/` separated)"
            ));
        }
        let service = service.trim();
        if !is_principal_name(service) {
            return Err(format!(
                "system account {n}: {service:?} is not a service principal's name"
            ));
        }
        if out.iter().any(|a| a.name == name) {
            return Err(format!("system account {n}: {name} is named twice"));
        }
        out.push(SystemAccountSpec {
            name: name.to_owned(),
            service: service.to_owned(),
        });
    }
    Ok(out)
}

/// A service principal as started: its name, role, identifiers and the grants it holds. Never its
/// token.
#[derive(Clone, Debug, PartialEq)]
pub struct ServicePrincipal {
    /// Its name, from the configuration.
    pub name: String,
    /// Its role.
    pub role: PrincipalRole,
    /// Its identity, `did:`-shaped: the principal its token answers as.
    pub principal: String,
    /// Its credential, `did:`-shaped.
    pub credential: String,
    /// The grants made to it at start, as `/sidecar` shows them.
    pub grants: Vec<Value>,
}

/// `max_legs` from its spelling: a whole number of at least one.
pub fn parse_max_legs(spec: &str) -> Result<usize, String> {
    match spec.trim().parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("{spec:?}: not a whole number of at least 1")),
    }
}

/// What a start can be asked for beyond the registry. `Default` is what the sidecar was before:
/// the operator under [`TEST_OPERATOR_TOKEN`] only, the kernel's default `max_legs`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Extra bearer tokens, each the operator ([`ENV_EXTRA_TOKENS`], [`parse_tokens`]).
    pub extra_tokens: Vec<String>,
    /// The book's `max_legs`; `None` is the kernel's default ([`ENV_MAX_LEGS`]).
    pub max_legs: Option<usize>,
    /// The notification recipients to register ([`ENV_RECIPIENTS`], [`parse_recipients`]).
    pub recipients: Vec<RecipientSpec>,
    /// The service principals to open ([`ENV_SERVICE_PRINCIPALS`], [`parse_principals`]). Empty
    /// by default here; the binary opens [`DEFAULT_PRINCIPALS`] unless configured otherwise
    /// ([`load_principals`]).
    pub services: Vec<PrincipalSpec>,
    /// The system accounts to open ([`ENV_SYSTEM_ACCOUNTS`], [`parse_system_accounts`]).
    pub system_accounts: Vec<SystemAccountSpec>,
    /// The passkeys' relying party ([`ENV_RP_ID`]); `None` is [`DEFAULT_RP_ID`].
    pub rp_id: Option<String>,
    /// Its origins ([`ENV_RP_ORIGINS`]); empty is [`DEFAULT_RP_ORIGINS`].
    pub rp_origins: Vec<String>,
}

/// The relying party's origins from their spelling, comma separated, trimmed, empty items
/// dropped.
pub fn parse_origins(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The sidecar's genesis, at `at`.
pub fn genesis_config(assets: Vec<AssetConfig>, at: u64) -> GenesisConfig {
    GenesisConfig {
        assets,
        operator_verifier: "0f".into(),
        deployment: hex::encode(DEPLOYMENT),
        decision_bound_ms: 60_000,
        review_service_level_ms: 3_600_000,
        default_deadlines_ms: ["reserve", "transfer", "redeem"]
            .into_iter()
            .map(|k| (k.to_owned(), DEFAULT_DEADLINE_MS))
            .collect(),
        genesis_op: hex::encode(GENESIS_OP),
        genesis_at: at,
        recovery: None,
        anchoring: None,
        opening_sources: std::collections::BTreeMap::new(),
        chainspec: None,
    }
}

/// A running sidecar: the replica (its ticker, forwarder and delivery worker run while it lives),
/// its surface, and what genesis founded.
pub struct Sidecar {
    /// The replica. Dropping it stops nothing; the process ending does.
    pub replica: Replica<MemStore, TestCore>,
    /// Its HTTP surface.
    pub router: Router,
    /// The operator, its credential and the issuance account.
    pub founded: Founded,
    /// The registry.
    pub assets: Vec<AssetConfig>,
    /// The most legs one transfer submission may carry: the book's `max_legs`.
    pub max_legs: usize,
    /// How many extra tokens are recognised as the operator (their values are not kept).
    pub extra_tokens: usize,
    /// The registered recipients, `(principal, endpoint)` (their secrets are not kept here).
    pub recipients: Vec<(String, String)>,
    /// The service principals, in the order they were named (their tokens are not kept).
    pub services: Vec<ServicePrincipal>,
    /// The system accounts: role, account and the service granted it.
    pub system_accounts: Vec<Value>,
    /// The passkeys' relying party and its origins.
    pub relying_party: (String, Vec<String>),
}

impl Sidecar {
    /// Found the book, start the replica and seed it: the operator's token and one issuance grant
    /// per asset. Nothing listens yet ([`Sidecar::serve`]).
    pub async fn start(assets: Vec<AssetConfig>) -> Result<Sidecar, String> {
        Sidecar::start_with(assets, &Options::default()).await
    }

    /// [`Sidecar::start`], with `options`: the extra operator tokens, the book's `max_legs` and
    /// the recipients, whose secrets are read from the process environment.
    pub async fn start_with(
        assets: Vec<AssetConfig>,
        options: &Options,
    ) -> Result<Sidecar, String> {
        Sidecar::start_with_lookup(assets, options, |v| std::env::var(v).ok()).await
    }

    /// [`Sidecar::start_with`], reading each recipient's secret and each service's token through
    /// `lookup` (a variable's value by its name). One that is not set fails the start, naming the
    /// variable.
    pub async fn start_with_lookup(
        assets: Vec<AssetConfig>,
        options: &Options,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Sidecar, String> {
        let clock: SharedClock = Arc::new(SystemClock);
        let all_assets = &assets;
        let mut genesis =
            genesis_config(assets.clone(), clock.now()).core_genesis(TEST_CORE_ROOT)?;
        if let Some(n) = options.max_legs {
            if n == 0 {
                return Err("max_legs: at least 1".into());
            }
            genesis.genesis.lifecycle.max_legs = n;
        }
        let max_legs = genesis.genesis.lifecycle.max_legs;
        let core = InProcessCore::in_memory(genesis, MemJournal::new());
        core.start()?;
        let founded = core.founded().ok_or("the core started without founding")?;

        // Every service's token, read and checked before anything starts: set, one word, and
        // no other principal's (never the operator's).
        let mut service_tokens: Vec<String> = Vec::new();
        // The configuration is checked as `parse_principals` checks it, for an `Options` built by
        // hand: one set of rules.
        check_principals(&options.services)
            .map_err(|e| format!("{ENV_SERVICE_PRINCIPALS}: {e}"))?;
        for svc in &options.services {
            if let Some(assets) = &svc.assets
                && let Some(a) = assets
                    .iter()
                    .find(|a| !all_assets.iter().any(|r| &r.id == *a))
            {
                return Err(format!(
                    "{ENV_SERVICE_PRINCIPALS}: {}: {a} is not an asset of the registry",
                    svc.name
                ));
            }
            // `check_principals` leaves one source: a variable, or the opted-in test token.
            let (token, source) = match &svc.token_env {
                Some(var) => (
                    lookup(var)
                        .ok_or_else(|| format!("{ENV_SERVICE_PRINCIPALS}: {var} is not set"))?,
                    var.clone(),
                ),
                None => (
                    test_principal_token(&svc.name),
                    format!("{}'s test token", svc.name),
                ),
            };
            let token = token.trim().to_owned();
            if token.is_empty() || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "{ENV_SERVICE_PRINCIPALS}: {source} is empty or holds whitespace"
                ));
            }
            if token == TEST_OPERATOR_TOKEN
                || options.extra_tokens.contains(&token)
                || service_tokens.contains(&token)
            {
                return Err(format!(
                    "{ENV_SERVICE_PRINCIPALS}: {source} is another principal's token"
                ));
            }
            service_tokens.push(token);
        }
        for a in &options.system_accounts {
            if !options.services.iter().any(|s| s.name == a.service) {
                return Err(format!(
                    "{ENV_SYSTEM_ACCOUNTS}: {} is for {}, which is not a service principal",
                    a.name, a.service
                ));
            }
        }

        let auth = Authenticator::new();
        auth.allow_holders(true);
        // Passkeys of one relying party (`F-028`), checked as `aledgent-ingress` checks its
        // configuration.
        let rp_id = options
            .rp_id
            .clone()
            .unwrap_or_else(|| DEFAULT_RP_ID.to_owned());
        let rp_origins = if options.rp_origins.is_empty() {
            parse_origins(DEFAULT_RP_ORIGINS)
        } else {
            options.rp_origins.clone()
        };
        let passkeys = PasskeysConfig {
            relying_parties: vec![RelyingPartyConfig {
                rp_id: rp_id.clone(),
                origins: rp_origins.clone(),
                registrations: true,
            }],
            require_user_verification: true,
            challenge_ttl_ms: 120_000,
            session_ttl_ms: 900_000,
            approvals: Default::default(),
            environment: None,
        }
        .passkeys(AuthnKey::new(TEST_AUTHN_KEY))
        .map_err(|e| format!("{ENV_RP_ID}/{ENV_RP_ORIGINS}: {e}"))?;
        auth.set_passkeys(passkeys);
        // The test token, then every extra one: the same operator, the same credential.
        let mut extra: Vec<&str> = Vec::new();
        for t in &options.extra_tokens {
            if t != TEST_OPERATOR_TOKEN && !extra.contains(&t.as_str()) {
                extra.push(t);
            }
        }
        for token in std::iter::once(TEST_OPERATOR_TOKEN).chain(extra.iter().copied()) {
            auth.add(&TokenEntry {
                token_sha256: hex::encode(Sha256::digest(token.as_bytes())),
                principal: founded.operator.to_did(),
                credential: founded.operator_credential.to_did(),
                kind: CredentialKind::Service,
            })?;
        }

        let config = Config {
            replica: "sidecar".into(),
            ..Config::default()
        };
        let store = MemStore::new(config.lease);
        // The recipients, before the delivery worker starts, through the code
        // `aledgent-ingress recipient` registers one with.
        let mut recipients: Vec<(String, String)> = Vec::new();
        for r in &options.recipients {
            // A name names its service principal, whose identifier is fixed by genesis and the
            // name (`service_principal`), so it is known before the service is opened.
            let principal = match r.principal.as_deref() {
                None => founded.operator.to_did(),
                Some(p) if p.starts_with("did:") => p.to_owned(),
                Some(p) if options.services.iter().any(|s| s.name == p) => {
                    service_principal(&founded, p).0
                }
                Some(p) => {
                    return Err(format!(
                        "{ENV_RECIPIENTS}: {p} is not a service principal \
                         ({ENV_SERVICE_PRINCIPALS})"
                    ));
                }
            };
            let recipient = Recipient::with_secret_env(
                principal.clone(),
                r.endpoint.clone(),
                &r.secret_env,
                &lookup,
            )
            .map_err(|e| format!("{ENV_RECIPIENTS}: {e}"))?;
            store
                .put_recipient(recipient)
                .await
                .map_err(|e| format!("{ENV_RECIPIENTS}: {e}"))?;
            recipients.push((principal, r.endpoint.clone()));
        }
        let transport = HttpTransport::new(Duration::from_secs(10))?;
        let replica = Replica::start(store, core, clock, auth, Arc::new(transport), config);
        let router = router(Arc::clone(&replica.shared));

        let sidecar = Sidecar {
            replica,
            router,
            founded,
            assets,
            max_legs,
            extra_tokens: extra.len(),
            recipients,
            services: Vec::new(),
            system_accounts: Vec::new(),
            relying_party: (rp_id, rp_origins),
        };
        for a in &sidecar.assets {
            let (s, v) = sidecar
                .call(
                    "POST",
                    "/v1/grants",
                    &[],
                    Some(json!({
                        "kind": "issuance",
                        "principal": sidecar.founded.operator.to_did(),
                        "subject": { "asset": a.id },
                        "ops": ["issue", "redeem"],
                        "per_window": { "length": 86_400_000u64, "limit": PER_DAY_LIMIT },
                        "assets": [a.id],
                    })),
                )
                .await;
            if s != StatusCode::OK {
                return Err(format!("the operator's issuance grant over {}: {v}", a.id));
            }
        }
        let (s, v) = sidecar
            .call(
                "POST",
                "/v1/grants",
                &[],
                Some(json!({
                    "kind": "operator",
                    "principal": sidecar.founded.operator.to_did(),
                    "subject": "delegated_accounts",
                    "ops": OPERATOR_OPS,
                    "per_window": { "length": 86_400_000u64, "limit": PER_DAY_LIMIT },
                })),
            )
            .await;
        if s != StatusCode::OK {
            return Err(format!("the operator's grant over delegated accounts: {v}"));
        }
        let mut sidecar = sidecar;
        for (svc, token) in options.services.iter().zip(&service_tokens) {
            let started = sidecar.open_service(svc, token).await?;
            sidecar.services.push(started);
        }
        for a in &options.system_accounts {
            sidecar.open_system_account(a).await?;
        }
        Ok(sidecar)
    }

    /// A command as the operator, `200` or an error naming `what`; its `result`.
    async fn seed(&self, what: &str, path: &str, key: &str, body: Value) -> Result<Value, String> {
        let (s, v) = self
            .call("POST", path, &[("client-key", key)], Some(body))
            .await;
        if s != StatusCode::OK {
            return Err(format!("{what}: {v}"));
        }
        Ok(v.get("result").cloned().unwrap_or(v))
    }

    /// Open the service principal `svc` and give it the grants of its role: its own organisation
    /// identity, opened by the operator, recognised under `token` as a service principal.
    async fn open_service(
        &self,
        svc: &PrincipalSpec,
        token: &str,
    ) -> Result<ServicePrincipal, String> {
        let name = svc.name.as_str();
        let what = |w: &str| format!("{ENV_SERVICE_PRINCIPALS}: {name}: {w}");
        // Opened straight through the core under an operation reference derived from the name
        // (`service_operation`), not one ingress mints at random, so that its identifiers are the
        // same on every start, as the operator's are.
        let op = service_operation(&self.founded, name);
        let now = self.replica.shared.clock.now();
        let input = CoreInput {
            key: InputKey(op.to_ref().0),
            body: InputBody::Command {
                caller: CallerBody {
                    principal: self.founded.operator.to_did(),
                    credential: self.founded.operator_credential.to_did(),
                    on_behalf_of: None,
                },
                operation: op.to_did(),
                received_at: now,
                now,
                command: Box::new(CommandBody::OpenIdentity(OpenIdentityBody {
                    kind: IdentityKindBody::Organisation,
                    acts_for: None,
                    verifier: hex::encode(format!("aledgent-sidecar/service/{name}")),
                    scheme: None,
                    reference: None,
                })),
            },
        };
        match self.replica.shared.core.apply(&input).await {
            Ok(CoreReply {
                outcome: Outcome::Decided { .. },
                ..
            }) => {}
            other => return Err(what(&format!("open_identity: {other:?}"))),
        }
        let (principal, credential) = service_principal(&self.founded, name);
        self.replica.shared.auth.add(&TokenEntry {
            token_sha256: hex::encode(Sha256::digest(token.as_bytes())),
            principal: principal.clone(),
            credential: credential.clone(),
            kind: CredentialKind::Service,
        })?;

        let window = json!({ "length": 86_400_000u64, "limit": svc.per_day_limit });
        let mut grants: Vec<Value> = Vec::new();
        match svc.role {
            PrincipalRole::IssuanceAuthority => {
                let assets: Vec<&str> = match &svc.assets {
                    Some(listed) => listed.iter().map(String::as_str).collect(),
                    None => self.assets.iter().map(|a| a.id.as_str()).collect(),
                };
                for id in assets {
                    let body = json!({
                        "kind": "issuance",
                        "principal": principal,
                        "subject": { "asset": id },
                        "ops": svc.ops,
                        "per_window": window,
                        "assets": [id],
                    });
                    let r = self
                        .seed(
                            &what(&format!("issuance grant over {id}")),
                            "/v1/grants",
                            &format!("sidecar-service-{name}-issuance-{id}"),
                            body.clone(),
                        )
                        .await?;
                    grants.push(granted(&r, body));
                }
            }
            PrincipalRole::OperatorAuthority => {
                let body = json!({
                    "kind": "operator",
                    "principal": principal,
                    "subject": "delegated_accounts",
                    "ops": svc.ops,
                    "per_window": window,
                });
                let r = self
                    .seed(
                        &what("operator grant over delegated accounts"),
                        "/v1/grants",
                        &format!("sidecar-service-{name}-operator"),
                        body.clone(),
                    )
                    .await?;
                grants.push(granted(&r, body));
            }
            PrincipalRole::IdentitiesPrincipal => {
                let body = json!({
                    "principal": principal,
                    "kinds": ["open_identity"],
                    "justification": format!(
                        "aledgent-sidecar: {name} plays the identities principal (REQ-ID-6, \
                         amendment 0025)"
                    ),
                });
                let r = self
                    .seed(
                        &what("the identities principal's role"),
                        "/v1/ops/operator-grants",
                        &format!("sidecar-service-{name}-identities"),
                        body,
                    )
                    .await?;
                grants.push(json!({
                    "grant": r.get("grant").cloned().unwrap_or(Value::Null),
                    "kind": "operator_surface",
                    "kinds": ["open_identity"],
                }));
            }
        }
        Ok(ServicePrincipal {
            name: name.to_owned(),
            role: svc.role,
            principal,
            credential,
            grants,
        })
    }

    /// Open a system account under the operator's identity, name it by its role and grant it,
    /// account-scoped and bounded, to the service that operates it (`ops/runbooks/system-accounts.md`
    /// steps 2 to 4).
    async fn open_system_account(&mut self, a: &SystemAccountSpec) -> Result<(), String> {
        let what = |w: &str| format!("{ENV_SYSTEM_ACCOUNTS}: {}: {w}", a.name);
        let operator = self.founded.operator.to_did();
        let opened = self
            .seed(
                &what("open_account"),
                "/v1/accounts",
                &format!("sidecar-system-{}", a.name),
                json!({ "owner": operator, "kind": "customer", "custody": "delegated" }),
            )
            .await?;
        let account = opened["account"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| what(&format!("no account in {opened}")))?;
        self.seed(
            &what("register_alias"),
            &format!("/v1/ops/accounts/{account}/aliases"),
            &format!("sidecar-system-alias-{}", a.name),
            json!({
                "source": SYSTEM_ROLE_SOURCE,
                "name": a.name,
                "justification": "aledgent-sidecar: a system account's role",
            }),
        )
        .await?;
        let svc = self
            .services
            .iter_mut()
            .find(|s| s.name == a.service)
            .ok_or_else(|| what("no such service principal"))?;
        let body = json!({
            "kind": "delegated",
            "principal": svc.principal,
            "subject": { "account": account },
            "ops": SYSTEM_ACCOUNT_OPS,
            "per_window": { "length": 86_400_000u64, "limit": PER_DAY_LIMIT },
        });
        let (s, v) = call(
            &self.router,
            "POST",
            "/v1/grants",
            Some(TEST_OPERATOR_TOKEN),
            &[("client-key", &format!("sidecar-system-grant-{}", a.name))],
            Some(body.clone()),
        )
        .await;
        if s != StatusCode::OK {
            return Err(what(&format!("the account-scoped grant: {v}")));
        }
        let r = v.get("result").cloned().unwrap_or(v);
        svc.grants.push(granted(&r, body));
        self.system_accounts.push(json!({
            "role": a.name,
            "account": account,
            "service": a.service,
        }));
        Ok(())
    }

    /// A request through the surface, in process, as the operator.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call(
            &self.router,
            method,
            path,
            Some(TEST_OPERATOR_TOKEN),
            headers,
            body,
        )
        .await
    }

    /// What genesis founded and how to call it, as JSON: public identifiers and the test token,
    /// the book's `max_legs`, the **number** of extra tokens (never their values), the
    /// recipients' number and endpoints (never their secrets), each service principal's
    /// identifiers and grants (never its token) and the system accounts.
    pub fn describe(&self) -> Value {
        json!({
            "operator": self.founded.operator.to_did(),
            "operator_credential": self.founded.operator_credential.to_did(),
            "issuance": self.founded.issuance.to_did(),
            "token": TEST_OPERATOR_TOKEN,
            "extra_tokens": self.extra_tokens,
            "max_legs": self.max_legs,
            "recipients": self.recipients.len(),
            "recipient_endpoints": self.recipients.iter()
                .map(|(p, e)| json!({ "principal": p, "endpoint": e }))
                .collect::<Vec<_>>(),
            "assets": self.assets.iter()
                .map(|a| format!("{}/{}", a.id, a.precision))
                .collect::<Vec<_>>(),
            "service_principals": self.services.iter()
                .map(|s| (s.name.clone(), json!({
                    "role": s.role.name(),
                    "principal": s.principal,
                    "credential": s.credential,
                    "grants": s.grants,
                })))
                .collect::<serde_json::Map<String, Value>>(),
            "system_accounts": self.system_accounts,
            "passkeys": {
                "rp_id": self.relying_party.0,
                "origins": self.relying_party.1,
            },
        })
    }

    /// The HTTP application: ingress's surface, and `GET /sidecar`, unauthenticated, answering
    /// [`Sidecar::describe`] without the token, so that a CI job can read the operator's
    /// identifier from the running container.
    pub fn app(&self) -> Router {
        let mut d = self.describe();
        if let Some(m) = d.as_object_mut() {
            m.remove("token");
        }
        let d = Arc::new(d);
        self.router.clone().merge(Router::new().route(
            "/sidecar",
            get(move || {
                let d = Arc::clone(&d);
                async move { axum::Json((*d).clone()) }
            }),
        ))
    }

    /// Serve [`Sidecar::app`] on `listener` until the process is interrupted.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> Result<(), String> {
        let app = self.app();
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| e.to_string())
    }
}

/// The operation reference the service principal named `name` is opened under: the operation
/// reference of the operator's command under the client key `sidecar-service-<name>`
/// (`REQ-OP-10`), so it depends on genesis and the name alone.
pub fn service_operation(founded: &Founded, name: &str) -> OperationId {
    operation_reference(
        &DEPLOYMENT,
        &KeyDomain::Principal(founded.operator),
        Surface::Command,
        &ClientKey::new(format!("sidecar-service-{name}").into_bytes()),
    )
}

/// The service principal named `name` and its credential, `did:`-shaped: the same on every start
/// of the same genesis, as the operator's are.
pub fn service_principal(founded: &Founded, name: &str) -> (String, String) {
    let op = service_operation(founded, name);
    (
        IdentityId::mint(&op, 0).to_did(),
        CredentialId::mint(&op, 1).to_did(),
    )
}

/// A grant as `/sidecar` shows it: its identifier, from the command's result, and its terms.
fn granted(result: &Value, mut terms: Value) -> Value {
    if let Some(m) = terms.as_object_mut() {
        m.insert(
            "grant".into(),
            result.get("grant").cloned().unwrap_or(Value::Null),
        );
    }
    terms
}

/// The address `aledgent-sidecar health` probes for a listen address: the same port on the
/// loopback when the listener is on every interface.
pub fn probe_address(listen: &str) -> Result<SocketAddr, String> {
    let addr = listen
        .to_socket_addrs()
        .map_err(|e| format!("{listen}: {e}"))?
        .next()
        .ok_or_else(|| format!("{listen}: no address"))?;
    Ok(if addr.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], addr.port()))
    } else {
        addr
    })
}

/// Whether the sidecar at `addr` is serving: `GET /metrics` as the operator answers `200`. It
/// listens only once seeded, so this is readiness, not just liveness. Blocking; for the health
/// check of a container with no shell.
pub fn health(addr: SocketAddr) -> Result<(), String> {
    let timeout = Duration::from_secs(3);
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("{addr}: {e}"))?;
    s.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET /metrics HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_OPERATOR_TOKEN}\r\n\
         Connection: close\r\n\r\n"
    );
    s.write_all(request.as_bytes())
        .map_err(|e| format!("{addr}: {e}"))?;
    let mut head = [0u8; 12];
    s.read_exact(&mut head)
        .map_err(|e| format!("{addr}: {e}"))?;
    let status = String::from_utf8_lossy(&head);
    if status.starts_with("HTTP/1.") && status.ends_with(" 200") {
        Ok(())
    } else {
        Err(format!("{addr}: {}", status.trim()))
    }
}
