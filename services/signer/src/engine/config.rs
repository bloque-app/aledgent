// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! How a component's configuration names an engine for each of its keys (`F-042`).
//!
//! A component **declares what it needs** in code: for each key, a name, a scheme and the kinds of
//! engine the key may use ([`KeyNeed`]). Its configuration has two maps, written once per file:
//!
//! ```json
//! {
//!   "engines": { "vault": { "vault_transit": { "vault": { … }, "mount": "transit" } } },
//!   "keys":    { "feed":  { "engine": "vault", "key": { "name": "lane-1", "version": 1 } } }
//! }
//! ```
//!
//! **One file serves several components, and each checks only its own keys.** The core host reads the
//! same deployment file as ingress. So [`check`] and [`open`] read only the entries of `keys` that the
//! component declares, and build only the engines those keys name. Every other entry stays unparsed JSON,
//! neither parsed nor refused: an engine kind this build lacks is an error only when one of the
//! component's own keys names it.
//!
//! [`check`] (offline, at load) refuses a key that is not given; an engine not declared, or of a kind this
//! build lacks; a scheme the engine does not offer; an engine kind the key may not use; a locator the
//! engine rejects; and the rules of the engine's connection. [`open`] (online, at start) then connects the
//! engines and opens each key, so a key that is absent, of another type or exportable stops the process.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::in_process::{self, InProcessEngine};
use super::sidecar::{self, SidecarEngine};
use super::{Engine, EngineKind, KeyHandle};
use crate::client::SignerEndpoint;
use crate::proto::Scheme;

/// A key a component needs: its name in the configuration, its scheme, and the kinds of engine it may use.
#[derive(Clone, Copy, Debug)]
pub struct KeyNeed {
    /// The name in the configuration's `keys`.
    pub name: &'static str,
    /// Its scheme.
    pub scheme: Scheme,
    /// The engine kinds it may use.
    pub kinds: &'static [EngineKind],
}

/// How a kind connects: its configuration, and whether development is allowed.
pub type ConnectFn = fn(config: &Value, development: bool) -> Result<Arc<dyn Engine>, String>;

/// A kind of engine, as the configuration names it: what is known offline, and how to connect.
pub struct KindInfo {
    /// The name in an `engines` entry (`"sidecar"`, `"in_process"`, `"vault_transit"`).
    pub name: &'static str,
    /// Its kind.
    pub kind: EngineKind,
    /// The schemes an engine of this kind can offer.
    pub schemes: &'static [Scheme],
    /// Check the engine's own configuration, offline. `development` allows what only development may.
    pub check_engine: fn(config: &Value, development: bool) -> Result<(), String>,
    /// Check a key's locator, offline.
    pub check_locator: fn(scheme: Scheme, locator: &Value) -> Result<(), String>,
    /// Connect the engine.
    pub connect: ConnectFn,
}

const ALL_SCHEMES: &[Scheme] = &[
    Scheme::KeyRing,
    Scheme::Clerk,
    Scheme::Ed25519,
    Scheme::Secp256k1,
];

/// The sidecar kind.
pub static SIDECAR: KindInfo = KindInfo {
    name: "sidecar",
    kind: EngineKind::Sidecar,
    schemes: ALL_SCHEMES,
    check_engine: |c, _| {
        SignerEndpoint::deserialize(c)
            .map(|_| ())
            .map_err(|e| format!("a sidecar is {{socket, client, token_file}}: {e}"))
    },
    check_locator: |_, l| sidecar::Locator::parse(l).map(|_| ()),
    connect: |c, _| {
        let endpoint = SignerEndpoint::deserialize(c).map_err(|e| e.to_string())?;
        SidecarEngine::connect(&endpoint)
            .map(|e| Arc::new(e) as Arc<dyn Engine>)
            .map_err(|e| e.to_string())
    },
};

/// The in-process kind.
pub static IN_PROCESS: KindInfo = KindInfo {
    name: "in_process",
    kind: EngineKind::InProcess,
    schemes: ALL_SCHEMES,
    check_engine: |c, _| match c.as_object() {
        Some(o) if o.is_empty() => Ok(()),
        _ => Err("an in_process engine takes no configuration: {}".into()),
    },
    check_locator: |_, l| in_process::Locator::parse(l).map(|_| ()),
    connect: |_, _| Ok(Arc::new(InProcessEngine) as Arc<dyn Engine>),
};

/// The kinds of engine this build knows (module documentation). Others are added with
/// [`Registry::with`], compiled in: nothing loads code.
pub struct Registry {
    kinds: Vec<&'static KindInfo>,
}

impl Registry {
    /// The built-in kinds this build has: the sidecar, in-process, and (feature `transit`) Vault Transit.
    pub fn builtin() -> Registry {
        #[allow(unused_mut)]
        let mut kinds: Vec<&'static KindInfo> = vec![&SIDECAR, &IN_PROCESS];
        #[cfg(feature = "transit")]
        kinds.push(&crate::transit::VAULT_TRANSIT);
        Registry { kinds }
    }

    /// With one more kind registered.
    pub fn with(mut self, kind: &'static KindInfo) -> Registry {
        self.kinds.push(kind);
        self
    }

    /// The kind named `name`.
    pub fn find(&self, name: &str) -> Option<&'static KindInfo> {
        self.kinds.iter().copied().find(|k| k.name == name)
    }

    fn names(&self) -> String {
        self.kinds
            .iter()
            .map(|k| k.name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The two maps of a component's configuration.
#[derive(Clone, Copy, Debug)]
pub struct Section<'a> {
    /// `engines`: name to `{ "<kind>": { config } }`.
    pub engines: &'a BTreeMap<String, Value>,
    /// `keys`: name to `{ "engine": "<name>", "key": { locator } }`.
    pub keys: &'a BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    engine: String,
    key: Value,
}

/// One key of the component, resolved offline.
struct Resolved<'a> {
    need: &'a KeyNeed,
    engine_name: String,
    kind: &'static KindInfo,
    engine_config: &'a Value,
    locator: Value,
}

fn resolve<'a>(
    registry: &Registry,
    section: &Section<'a>,
    need: &'a KeyNeed,
    development: bool,
) -> Result<Resolved<'a>, String> {
    let at = format!("keys.{}", need.name);
    let raw = section
        .keys
        .get(need.name)
        .ok_or_else(|| format!("{at}: not given"))?;
    let entry = Entry::deserialize(raw)
        .map_err(|e| format!("{at}: {{\"engine\": \"<name>\", \"key\": {{…}}}}: {e}"))?;
    let engine_config = section.engines.get(&entry.engine).ok_or_else(|| {
        format!(
            "{at}: the engine {:?} is not declared in \"engines\"",
            entry.engine
        )
    })?;
    let (kind_name, config) = match engine_config.as_object() {
        Some(o) if o.len() == 1 => o.iter().next().map(|(k, v)| (k.as_str(), v)).ok_or("")?,
        _ => {
            return Err(format!(
                "{at}: engines.{} is {{\"<kind>\": {{…}}}}, one kind",
                entry.engine
            ));
        }
    };
    let kind = registry.find(kind_name).ok_or_else(|| {
        format!(
            "{at}: engines.{} is of kind {kind_name:?}, which this build does not have (it has: {})",
            entry.engine,
            registry.names()
        )
    })?;
    if !kind.schemes.contains(&need.scheme) {
        return Err(format!(
            "{at}: the engine {:?} ({}) does not offer {:?}; it offers {:?}",
            entry.engine, kind.name, need.scheme, kind.schemes
        ));
    }
    if !need.kinds.contains(&kind.kind) {
        let allowed: Vec<&str> = need.kinds.iter().map(|k| k.as_str()).collect();
        return Err(format!(
            "{at}: the engine {:?} is of kind {} ({}); this key may use: {}",
            entry.engine,
            kind.kind.as_str(),
            kind.name,
            allowed.join(", ")
        ));
    }
    (kind.check_engine)(config, development)
        .map_err(|e| format!("{at}: engines.{}: {e}", entry.engine))?;
    (kind.check_locator)(need.scheme, &entry.key).map_err(|e| format!("{at}: {e}"))?;
    Ok(Resolved {
        need,
        engine_name: entry.engine,
        kind,
        engine_config: config,
        locator: entry.key,
    })
}

/// Check the component's own keys offline (module documentation).
pub fn check(
    registry: &Registry,
    section: &Section,
    needs: &[KeyNeed],
    development: bool,
) -> Result<(), String> {
    for need in needs {
        resolve(registry, section, need, development)?;
    }
    Ok(())
}

/// The component's keys, open.
pub struct OpenKeys {
    handles: BTreeMap<&'static str, Arc<dyn KeyHandle>>,
    engines: Vec<Arc<dyn Engine>>,
}

impl OpenKeys {
    /// The key `name`.
    pub fn handle(&self, name: &str) -> Option<Arc<dyn KeyHandle>> {
        self.handles.get(name).cloned()
    }

    /// The engines in use, once each: for readiness and metrics.
    pub fn engines(&self) -> &[Arc<dyn Engine>] {
        &self.engines
    }

    /// Whether every engine in use is ready.
    pub fn ready(&self) -> bool {
        self.engines.iter().all(|e| e.health().ready)
    }

    /// The metrics of every engine in use, as Prometheus text.
    pub fn metrics_text(&self) -> String {
        self.engines.iter().map(|e| e.metrics_text()).collect()
    }
}

/// Check and connect: open every one of the component's own keys (module documentation).
pub fn open(
    registry: &Registry,
    section: &Section,
    needs: &[KeyNeed],
    development: bool,
) -> Result<OpenKeys, String> {
    let mut connected: BTreeMap<String, Arc<dyn Engine>> = BTreeMap::new();
    let mut handles = BTreeMap::new();
    for need in needs {
        let r = resolve(registry, section, need, development)?;
        let engine = match connected.get(&r.engine_name) {
            Some(e) => Arc::clone(e),
            None => {
                let e = (r.kind.connect)(r.engine_config, development)
                    .map_err(|e| format!("keys.{}: engines.{}: {e}", need.name, r.engine_name))?;
                connected.insert(r.engine_name.clone(), Arc::clone(&e));
                e
            }
        };
        let h = engine
            .open(r.need.scheme, &r.locator)
            .map_err(|e| format!("keys.{}: {e}", need.name))?;
        handles.insert(need.name, h);
    }
    Ok(OpenKeys {
        handles,
        engines: connected.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ED: KeyNeed = KeyNeed {
        name: "feed",
        scheme: Scheme::Ed25519,
        kinds: &[
            EngineKind::Sidecar,
            EngineKind::InProcess,
            EngineKind::ApiProvider,
        ],
    };
    const CORE: [KeyNeed; 2] = [
        KeyNeed {
            name: "key_ring",
            scheme: Scheme::KeyRing,
            kinds: &[EngineKind::Sidecar],
        },
        KeyNeed {
            name: "agent_authority",
            scheme: Scheme::Ed25519,
            kinds: &[EngineKind::Sidecar],
        },
    ];

    fn maps(engines: Value, keys: Value) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
        let m = |v: Value| -> BTreeMap<String, Value> {
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        (m(engines), m(keys))
    }

    fn sidecar() -> Value {
        json!({ "sidecar": { "socket": "/run/s.sock", "client": "c", "token_file": "/t" } })
    }

    #[test]
    fn nfr_8_a_key_whose_scheme_or_kind_the_engine_cannot_serve_is_refused_at_load() {
        let r = Registry::builtin();
        let (e, k) = maps(
            json!({ "local": sidecar(), "files": { "in_process": {} } }),
            json!({
                "feed": { "engine": "local", "key": { "id": "ingress-feed" } },
                "key_ring": { "engine": "files", "key": { "file": "/k" } },
                "agent_authority": { "engine": "local", "key": { "id": "authority" } },
            }),
        );
        let s = Section {
            engines: &e,
            keys: &k,
        };
        assert_eq!(check(&r, &s, &[ED], false), Ok(()));
        // In-process offers key_ring, but the core host's key ring may use the sidecar only.
        let err = check(&r, &s, &CORE, false).unwrap_err();
        assert!(
            err.contains("keys.key_ring")
                && err.contains("kind in_process")
                && err.contains("may use: sidecar"),
            "{err}"
        );
    }

    #[test]
    fn nfr_8_the_check_reads_only_the_keys_the_component_declares() {
        let r = Registry::builtin();
        // Another component's key names a kind this build lacks, and has no locator at all.
        let (e, k) = maps(
            json!({ "local": sidecar(), "elsewhere": { "no_such_engine": { "x": 1 } } }),
            json!({
                "feed": { "engine": "elsewhere", "key": 7 },
                "key_ring": { "engine": "local", "key": { "id": "keyring" } },
                "agent_authority": { "engine": "local", "key": { "id": "authority" } },
            }),
        );
        let s = Section {
            engines: &e,
            keys: &k,
        };
        assert_eq!(check(&r, &s, &CORE, false), Ok(()));
        // The component that does declare `feed` is told.
        let err = check(&r, &s, &[ED], false).unwrap_err();
        assert!(
            err.contains("keys.feed") && err.contains("no_such_engine"),
            "{err}"
        );
        // A key the component needs and the file does not give.
        let (e2, k2) = maps(json!({ "local": sidecar() }), json!({}));
        let err = check(
            &r,
            &Section {
                engines: &e2,
                keys: &k2,
            },
            &CORE,
            false,
        )
        .unwrap_err();
        assert!(err.contains("keys.key_ring: not given"), "{err}");
    }

    #[test]
    fn nfr_8_an_in_process_key_is_a_file_and_never_the_environment() {
        let r = Registry::builtin();
        let (e, k) = maps(
            json!({ "files": { "in_process": {} } }),
            json!({ "feed": { "engine": "files", "key": { "env": "FEED_KEY" } } }),
        );
        let err = check(
            &r,
            &Section {
                engines: &e,
                keys: &k,
            },
            &[ED],
            true,
        )
        .unwrap_err();
        assert!(err.contains("keys.feed") && err.contains("env"), "{err}");
        let (e, k) = maps(
            json!({ "files": { "in_process": {} } }),
            json!({ "feed": { "engine": "files", "key": { "file": "/etc/feed.key" } } }),
        );
        // A file is accepted in production for a key that may be in process.
        assert_eq!(
            check(
                &r,
                &Section {
                    engines: &e,
                    keys: &k
                },
                &[ED],
                false
            ),
            Ok(())
        );
    }

    #[test]
    fn a_malformed_entry_names_the_key() {
        let r = Registry::builtin();
        let (e, k) = maps(
            json!({ "local": sidecar(), "two": { "sidecar": {}, "in_process": {} } }),
            json!({ "feed": { "engine": "ghost", "key": {} } }),
        );
        let s = Section {
            engines: &e,
            keys: &k,
        };
        assert!(
            check(&r, &s, &[ED], false)
                .unwrap_err()
                .contains("not declared")
        );
        let (_, k) = maps(json!({}), json!({ "feed": { "engine": "two", "key": {} } }));
        assert!(
            check(
                &r,
                &Section {
                    engines: &e,
                    keys: &k
                },
                &[ED],
                false
            )
            .unwrap_err()
            .contains("one kind")
        );
        let (_, k) = maps(
            json!({}),
            json!({ "feed": { "engine": "local", "id": "x" } }),
        );
        assert!(
            check(
                &r,
                &Section {
                    engines: &e,
                    keys: &k
                },
                &[ED],
                false
            )
            .unwrap_err()
            .contains("keys.feed")
        );
    }

    #[cfg(feature = "transit")]
    #[test]
    fn nfr_8_transit_serves_the_feed_key_and_nothing_of_the_core_hosts() {
        let r = Registry::builtin();
        let transit = json!({ "vault_transit": {
            "vault": { "address": "https://vault.example",
                       "auth": { "aws_iam": { "mount": "aws", "role": "r", "region": "us-east-1" } } },
            "mount": "transit" } });
        let (e, k) = maps(
            json!({ "vault": transit }),
            json!({
                "feed": { "engine": "vault", "key": { "name": "lane-1", "version": 1 } },
                "key_ring": { "engine": "vault", "key": { "name": "lane-1", "version": 1 } },
                "agent_authority": { "engine": "vault", "key": { "name": "lane-1", "version": 1 } },
            }),
        );
        let s = Section {
            engines: &e,
            keys: &k,
        };
        assert_eq!(check(&r, &s, &[ED], false), Ok(()));
        // Transit lacks the key ring's scheme, and the core host's Ed25519 keys may not use it.
        let err = check(&r, &s, &CORE[..1], false).unwrap_err();
        assert!(
            err.contains("keys.key_ring") && err.contains("does not offer"),
            "{err}"
        );
        let err = check(&r, &s, &CORE[1..], false).unwrap_err();
        assert!(
            err.contains("keys.agent_authority")
                && err.contains("kind api_provider")
                && err.contains("may use: sidecar"),
            "{err}"
        );
        // A version is required.
        let (e2, k2) = maps(
            json!({ "vault": transit }),
            json!({ "feed": { "engine": "vault", "key": { "name": "lane-1" } } }),
        );
        let err = check(
            &r,
            &Section {
                engines: &e2,
                keys: &k2,
            },
            &[ED],
            false,
        )
        .unwrap_err();
        assert!(err.contains("pin the key version"), "{err}");
        // Outside development a Vault is https and logs in with AWS IAM.
        let (e3, _) = maps(
            json!({ "vault": { "vault_transit": { "vault": { "address": "http://v",
                "auth": { "token_file": { "path": "/t" } } }, "mount": "transit" } } }),
            json!({}),
        );
        let err = check(
            &r,
            &Section {
                engines: &e3,
                keys: &k2,
            },
            &[ED],
            false,
        )
        .unwrap_err();
        assert!(err.contains("development only"), "{err}");
    }
}
