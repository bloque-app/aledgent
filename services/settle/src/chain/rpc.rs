// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement layer through the embedded smoldot light client (feature `light-client`, the
//! default: smoldot-light 2.1 behind subxt's RPC client, [`super::light`]), or over a node's RPC when the configuration opts into it — [`RpcChain`]. Both speak
//! the same JSON-RPC to the same client; only the transport differs.
//!
//! **The venue is the settlement contract** (`F-026`, `F-027`). Its storage is a child trie of
//! `pallet-contracts`, and **smoldot 0.19 served no child-trie read**: `childstate_getStorage`
//! answered "not implemented", and `chainHead_v1_storage` with a `childTrie` "not supported yet"
//! (`smoldot-light` 0.17.2, `json_rpc_service/background.rs`); the reads were built around that and
//! are unchanged on smoldot-light 2.1. A runtime call is served, and
//! smoldot proves every storage read it makes, child tries included. So the contract's inbox,
//! head, registration and positions are read with **`ContractsApi_call`**, a dry run of the
//! contract's read messages (`inbox_entry(seq)`, `head()`, `registration()`, `position(asset)`)
//! at the finalized block (`state_call` with that block's hash). The messages return the stored
//! values as stored: an inbox entry's bytes, and so its digest, are the storage's.
//!
//! **Chain state is read through the live metadata** (`F-016` plan; Kreivo's runtime): nonces
//! and native balances (`System::Account`), what the contract account holds (`Assets::Account`),
//! and what each bound asset location is (`Assets::Asset`: status and sufficiency;
//! `Assets::Metadata`: symbol, name, decimals; `DEC-20`). Those are plain storage, not a child
//! trie, so the light client serves and proves them: no contract call and no dry-run node is
//! involved. An asset that does not fit the runtime's type is an error, never a fallback.
//!
//! **Signing** (`F-027` T-027-03): a batch goes out as one `Contracts::call` of the contract's
//! `execute`, in a v5 general transaction authorised by `PassAuthenticate`, built with
//! [`super::kreivo`] from the runtime version at the best head, the genesis hash, the era's birth
//! block hash, and the challenge's context at the finalized head. **The era is born at that same
//! finalized head**, whose hash the adapter holds: nothing is looked up by block number, which the
//! embedded smoldot does not answer (`chain_getBlockHash` with a number: "no block N", F-026 NOTES,
//! "Against Kreivo main (0.17), and through aledgent", finding 4). For the same reason a
//! transaction is looked for in finalized blocks by walking parent hashes back from the finalized
//! head, never by number. The batch's gas limit is the adapter's, from a dry run of the same
//! `execute` (`ContractsApi_call` as the lane's PASS account, at the finalized block).
//!
//! **Run against Kreivo by the zombienet suite** (`T-027-06`, `e2e/README.md`), over the light
//! client and over a node's RPC; `tests/live_node.rs` drives it against a local chain, ignored by
//! default.

use scale_value::{Composite, Primitive, Value, ValueDef};
use subxt::backend::legacy::LegacyRpcMethods;
use subxt::backend::rpc::RpcClient;
use subxt::config::Header as _;
use subxt::dynamic::DecodedValueThunk;
use subxt::tx::SubmittableTransaction;
use subxt::utils::H256;
use subxt::{OnlineClient, PolkadotConfig};

use ledger_core::settlement::Venue;
use ledger_core::settlement::assets::{LAYER_TEXT_MAX, LayerAssetFacts, LayerAssetStatus};
use parity_scale_codec::{Decode, Encode};

use super::kreivo::{self, CallLimits, TxParams};
use super::{
    AccountId32, BatchCall, BlockRef, Chain, ChainAsset, ChainError, ChainResult, DryRun,
    InboxRead, Included, InstanceView, Mortality, RuntimeInfo, SignedExtrinsic,
};
use crate::signer::Signer;

/// The runtime's names, and the deployments' contracts.
#[derive(Clone, Debug)]
pub struct Names {
    /// `pallet-contracts` (`Contracts`).
    pub contracts: String,
    /// The assets pallet the contract moves (`Assets`).
    pub assets: String,
    /// Each deployment's contract account, by its instance: the deployment's own, and every
    /// deployment a withdrawal may name (`REQ-SL-6`).
    pub deployments: Vec<(u32, AccountId32)>,
}

impl Names {
    fn contract_of(&self, instance: u32) -> Option<AccountId32> {
        self.deployments
            .iter()
            .find(|(i, _)| *i == instance)
            .map(|(_, c)| *c)
    }

    fn instance_of(&self, contract: &AccountId32) -> Option<u32> {
        self.deployments
            .iter()
            .find(|(_, c)| c == contract)
            .map(|(i, _)| *i)
    }
}

/// The settlement layer over RPC. See the module documentation.
pub struct RpcChain {
    client: OnlineClient<PolkadotConfig>,
    rpc: LegacyRpcMethods<PolkadotConfig>,
    names: Names,
    assets: Vec<(ChainAsset, Value<()>)>,
    /// A node's RPC for dry runs only ([`RpcChain::with_dry_runs_over`]).
    dry_runs: Option<DryRunNode>,
    /// Every transaction submitted and still watched, by hash: see [`Chain::submit`].
    watching: std::sync::Mutex<std::collections::HashMap<[u8; 32], tokio::task::JoinHandle<()>>>,
    /// The last extrinsic limit read, and the block it was read at ([`Chain::extrinsic_limit`]).
    limit_at: std::sync::Mutex<Option<([u8; 32], kreivo::Weight)>>,
}

/// The node dry runs go to, and the connection to it: replaced by a new one whenever a dry run
/// fails over it. A websocket the node or the network closed stays closed ("restart required"):
/// seen on the zombienet fleet, where a minute and a half without peers left every later dry run
/// failing, and so nothing signed, for the rest of the process's life.
struct DryRunNode {
    url: String,
    rpc: std::sync::Mutex<LegacyRpcMethods<PolkadotConfig>>,
}

impl DryRunNode {
    async fn connect(url: &str) -> ChainResult<LegacyRpcMethods<PolkadotConfig>> {
        let rpc = RpcClient::from_insecure_url(url)
            .await
            .map_err(unavailable)?;
        Ok(LegacyRpcMethods::<PolkadotConfig>::new(rpc))
    }

    fn current(&self) -> LegacyRpcMethods<PolkadotConfig> {
        self.rpc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The runtime API `method` at `at`; on a failure, once more over a new connection.
    async fn call(&self, method: &str, args: &[u8], at: H256) -> ChainResult<Vec<u8>> {
        let first = self
            .current()
            .state_call(method, Some(args), Some(at))
            .await;
        let Err(e) = first else {
            return first.map_err(unavailable);
        };
        tracing::warn!(error = %e, method, "a call failed over the dry-run node's RPC; connecting to it again");
        let fresh = Self::connect(&self.url).await?;
        *self
            .rpc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh.clone();
        fresh
            .state_call(method, Some(args), Some(at))
            .await
            .map_err(unavailable)
    }
}

fn unavailable(e: impl std::fmt::Display) -> ChainError {
    ChainError::Unavailable(e.to_string())
}

fn invalid(e: impl std::fmt::Display) -> ChainError {
    ChainError::Invalid(e.to_string())
}

/// Parse a configured asset into the runtime value it names.
pub fn parse_asset(a: &ChainAsset) -> ChainResult<Value<()>> {
    let (v, rest) = scale_value::stringify::from_str(&a.0);
    let v = v.map_err(|e| invalid(format!("the asset {} does not parse: {e}", a.0)))?;
    if !rest.trim().is_empty() {
        return Err(invalid(format!("the asset {} does not parse", a.0)));
    }
    Ok(v)
}

fn field<'a>(v: &'a Value<u32>, name: &str) -> Option<&'a Value<u32>> {
    match &v.value {
        ValueDef::Composite(Composite::Named(fields)) => {
            fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
        }
        _ => None,
    }
}

fn number(v: &Value<u32>) -> Option<u128> {
    match &v.value {
        ValueDef::Primitive(Primitive::U128(n)) => Some(*n),
        // A newtype or a compact wrapper around a number.
        ValueDef::Composite(c) => {
            let vals: Vec<&Value<u32>> = c.values().collect();
            if vals.len() == 1 {
                number(vals[0])
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A byte string as scale-value decodes one (`BoundedVec<u8, _>`): a sequence of numbers, perhaps
/// inside a newtype.
fn byte_string(v: &Value<u32>) -> Option<Vec<u8>> {
    match &v.value {
        ValueDef::Composite(c) => {
            let vals: Vec<&Value<u32>> = c.values().collect();
            if vals.len() == 1 {
                if let ValueDef::Composite(_) = vals[0].value {
                    return byte_string(vals[0]);
                }
            }
            vals.iter()
                .map(|x| number(x).and_then(|n| u8::try_from(n).ok()))
                .collect()
        }
        ValueDef::Primitive(Primitive::String(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

fn variant_name(v: &Value<u32>) -> Option<&str> {
    match &v.value {
        ValueDef::Variant(var) => Some(var.name.as_str()),
        _ => None,
    }
}

fn account_value(a: &AccountId32) -> Value<()> {
    Value::from_bytes(a)
}

impl RpcChain {
    /// Connect through subxt's embedded smoldot light client (feature `light-client`, the
    /// default): to the chain `chain_spec` describes, as a parachain of `relay_chain_spec` when one
    /// is given (Kreivo on Kusama), or as a chain of its own when not. Both are chain specs'
    /// JSON text, with their boot nodes.
    #[cfg(feature = "light-client")]
    pub async fn light(
        chain_spec: &str,
        relay_chain_spec: Option<&str>,
        names: Names,
        assets: &[ChainAsset],
    ) -> ChainResult<RpcChain> {
        let rpc = super::light::LightRpc::new(chain_spec, relay_chain_spec).map_err(unavailable)?;
        Self::over(RpcClient::new(rpc), names, assets).await
    }

    /// Connect to a node's RPC at `url` (`ws://…`, `wss://…`): the opt-in fallback, chosen only by
    /// `"connection": "rpc"` in the configuration, never by the adapter itself.
    pub async fn connect(url: &str, names: Names, assets: &[ChainAsset]) -> ChainResult<RpcChain> {
        let rpc_client = RpcClient::from_insecure_url(url)
            .await
            .map_err(unavailable)?;
        Self::over(rpc_client, names, assets).await
    }

    async fn over(
        rpc_client: RpcClient,
        names: Names,
        assets: &[ChainAsset],
    ) -> ChainResult<RpcChain> {
        let client = OnlineClient::<PolkadotConfig>::from_rpc_client(rpc_client.clone())
            .await
            .map_err(unavailable)?;
        let rpc = LegacyRpcMethods::<PolkadotConfig>::new(rpc_client);
        let assets = assets
            .iter()
            .map(|a| {
                // Both forms must hold: the runtime value, and Kreivo's asset id for the contract.
                kreivo::parse_asset(&a.0).map_err(invalid)?;
                Ok((a.clone(), parse_asset(a)?))
            })
            .collect::<ChainResult<Vec<_>>>()?;
        Ok(RpcChain {
            client,
            rpc,
            names,
            assets,
            dry_runs: None,
            watching: std::sync::Mutex::default(),
            limit_at: std::sync::Mutex::default(),
        })
    }

    /// Run batches' dry runs (and only those) through the node's RPC at `url` (`F-027` NOTES,
    /// decisions 36 and 42). Over the embedded smoldot a batch of several calls cannot be dry-run:
    /// once the call has finished smoldot recalculates the root of the contract's child trie, and
    /// the node's call proof lacks what that needs: smoldot fails the call and bans that node. A
    /// dry run decides nothing but a
    /// batch's limits; every read that decides an outcome stays on this client.
    pub async fn with_dry_runs_over(mut self, url: &str) -> ChainResult<RpcChain> {
        let rpc = DryRunNode::connect(url).await?;
        self.dry_runs = Some(DryRunNode {
            url: url.to_owned(),
            rpc: std::sync::Mutex::new(rpc),
        });
        Ok(self)
    }

    /// `System::BlockWeights`' normal-class `max_extrinsic` from the runtime's metadata as it is
    /// built in the state of block `at` (`Metadata_metadata_at_version(15)`: the constant is
    /// evaluated then, against that state). Asked of the dry-run node when there is one (the
    /// metadata is large, and that node is trusted for a batch's limits already), else of this
    /// client.
    async fn limit_in_state(&self, at: H256) -> ChainResult<kreivo::Weight> {
        let version = 15u32.encode();
        let bytes = match &self.dry_runs {
            Some(node) => {
                node.call("Metadata_metadata_at_version", &version, at)
                    .await?
            }
            None => self
                .rpc
                .state_call("Metadata_metadata_at_version", Some(&version), Some(at))
                .await
                .map_err(unavailable)?,
        };
        // `Option<OpaqueMetadata>`: the metadata's own bytes, length-prefixed.
        let opaque = Option::<Vec<u8>>::decode(&mut &bytes[..])
            .map_err(invalid)?
            .ok_or_else(|| invalid("the runtime has no metadata at version 15"))?;
        let metadata = subxt::Metadata::decode(&mut &opaque[..]).map_err(invalid)?;
        let constant = metadata
            .pallet_by_name("System")
            .and_then(|p| p.constant_by_name("BlockWeights"))
            .ok_or_else(|| invalid("the runtime's metadata has no System::BlockWeights"))?;
        kreivo::normal_max_extrinsic(constant.value()).map_err(invalid)
    }

    fn asset_value(&self, a: &ChainAsset) -> ChainResult<Value<()>> {
        match self.assets.iter().find(|(c, _)| c == a) {
            Some((_, v)) => Ok(v.clone()),
            None => parse_asset(a),
        }
    }

    fn contract(&self, instance: u32) -> ChainResult<AccountId32> {
        self.names
            .contract_of(instance)
            .ok_or_else(|| invalid(format!("no contract is configured for instance {instance}")))
    }

    async fn block(&self, hash: H256) -> ChainResult<BlockRef> {
        let header = self
            .rpc
            .chain_get_header(Some(hash))
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable("no header"))?;
        Ok(BlockRef {
            number: header.number().into(),
            hash: hash.0,
        })
    }

    async fn fetch(
        &self,
        at: &BlockRef,
        pallet: &str,
        entry: &str,
        keys: Vec<Value<()>>,
    ) -> ChainResult<Option<DecodedValueThunk>> {
        self.client
            .storage()
            .at(H256(at.hash))
            .fetch(&subxt::dynamic::storage(pallet, entry, keys))
            .await
            .map_err(invalid)
    }

    /// A read message of `instance`'s contract, dry-run at block `at`: its return value.
    async fn read<T: Decode>(
        &self,
        at: &BlockRef,
        instance: u32,
        input: Vec<u8>,
    ) -> ChainResult<T> {
        let contract = self.contract(instance)?;
        let args = kreivo::contracts_api_call_args(&contract, &contract, &input);
        let bytes = self
            .rpc
            .state_call("ContractsApi_call", Some(&args), Some(H256(at.hash)))
            .await
            .map_err(unavailable)?;
        let data = kreivo::contract_exec_data(&bytes).map_err(invalid)?;
        kreivo::message_result::<T>(&data).map_err(invalid)
    }

    /// The runtime's metadata at block `at`: the newest version it serves, as subxt reads it when
    /// it connects.
    async fn metadata_at(&self, at: H256) -> ChainResult<subxt::Metadata> {
        use subxt::backend::BackendExt;
        let backend = self.client.backend();
        for version in [16, 15] {
            if let Ok(m) = backend.metadata_at_version(version, at).await {
                return Ok(m);
            }
        }
        backend.legacy_metadata(at).await.map_err(unavailable)
    }

    /// The runtime API `method` with SCALE-encoded `args`, at block `at`, through this client
    /// (`state_call`): over the light client, a call proof from a peer, executed locally. What
    /// [`Chain::dry_run`] runs when no dry-run node is configured; public so that the zombienet
    /// suite's light-client probe (`examples/lc_dryrun_probe.rs`) runs exactly this path.
    pub async fn runtime_call(
        &self,
        method: &str,
        args: &[u8],
        at: [u8; 32],
    ) -> ChainResult<Vec<u8>> {
        self.rpc
            .state_call(method, Some(args), Some(H256(at)))
            .await
            .map_err(unavailable)
    }

    /// Whether `instance`'s contract is instantiated at `at`.
    async fn instantiated(&self, at: &BlockRef, instance: u32) -> ChainResult<bool> {
        let contract = self.contract(instance)?;
        Ok(self
            .fetch(
                at,
                &self.names.contracts,
                "ContractInfoOf",
                vec![account_value(&contract)],
            )
            .await?
            .is_some())
    }
}

impl Chain for RpcChain {
    async fn runtime(&self) -> ChainResult<RuntimeInfo> {
        // The runtime at the best head, which is what a transaction signed now is checked
        // against, and its metadata: on a runtime upgrade (T-027-07), the new runtime's metadata
        // is read at that head before anything else is read or signed with it.
        let best = self
            .rpc
            .chain_get_block_hash(None)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable("no best block"))?;
        let version = self
            .rpc
            .state_get_runtime_version(Some(best))
            .await
            .map_err(unavailable)?;
        let known = self.client.runtime_version();
        if (known.spec_version, known.transaction_version)
            != (version.spec_version, version.transaction_version)
        {
            let metadata = self.metadata_at(best).await?;
            self.client.set_metadata(metadata);
            self.client
                .set_runtime_version(subxt::client::RuntimeVersion {
                    spec_version: version.spec_version,
                    transaction_version: version.transaction_version,
                });
            tracing::info!(
                from = known.spec_version,
                to = version.spec_version,
                format = ?kreivo::TxFormat::of(version.spec_version).map(|f| f.kreivo),
                "the settlement layer's runtime changed; its metadata is reloaded"
            );
        }
        let metadata = self.client.metadata();
        let extensions = metadata
            .extrinsic()
            .transaction_extensions_by_version(kreivo::EXTENSION_VERSION)
            .map(|it| it.map(|e| e.identifier().to_owned()).collect())
            .unwrap_or_default();
        let block_hash_count = self
            .client
            .constants()
            .at(&subxt::dynamic::constant("System", "BlockHashCount"))
            .ok()
            .and_then(|t| t.to_value().ok())
            .and_then(|v| number(&v))
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(4);
        Ok(RuntimeInfo {
            genesis: self.client.genesis_hash().0,
            spec_version: version.spec_version,
            extensions,
            pallet: metadata.pallet_by_name(&self.names.contracts).is_some(),
            block_hash_count,
        })
    }

    async fn finalized(&self) -> ChainResult<BlockRef> {
        let hash = self
            .rpc
            .chain_get_finalized_head()
            .await
            .map_err(unavailable)?;
        self.block(hash).await
    }

    async fn best(&self) -> ChainResult<BlockRef> {
        let hash = self
            .rpc
            .chain_get_block_hash(None)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable("no best block"))?;
        self.block(hash).await
    }

    async fn instance(&self, at: &BlockRef, instance: u32) -> ChainResult<Option<InstanceView>> {
        if !self.instantiated(at, instance).await? {
            return Ok(None);
        }
        let registration: kreivo::Registration = self
            .read(at, instance, kreivo::selector::REGISTRATION.to_vec())
            .await?;
        let head: kreivo::Head = self
            .read(at, instance, kreivo::selector::HEAD.to_vec())
            .await?;
        Ok(Some(InstanceView {
            keys: registration.keys,
            next_seq: head.next_seq,
            pruned_to: head.pruned_to,
            last_anchor: head.last_anchor,
        }))
    }

    async fn inbox(
        &self,
        at: &BlockRef,
        instance: u32,
        from: u64,
        count: u32,
    ) -> ChainResult<Vec<(u64, InboxRead)>> {
        let Some(view) = self.instance(at, instance).await? else {
            return Ok(Vec::new());
        };
        let to = from.saturating_add(u64::from(count)).min(view.next_seq);
        let mut out = Vec::new();
        for seq in from..to {
            let entry: Option<kreivo::InboxEntry> = self
                .read(
                    at,
                    instance,
                    kreivo::message(kreivo::selector::INBOX_ENTRY, &seq),
                )
                .await?;
            let read = match entry {
                // The message returns the stored value: its encoding is the stored bytes.
                Some(e) => InboxRead::Entry(
                    kreivo::inbox_entry(e.encode(), |c| self.names.instance_of(c))
                        .map_err(invalid)?,
                ),
                None if seq < view.pruned_to => InboxRead::Pruned,
                None => InboxRead::Absent,
            };
            out.push((seq, read));
        }
        Ok(out)
    }

    async fn asset_metadata(
        &self,
        at: &BlockRef,
        asset: &ChainAsset,
    ) -> ChainResult<Option<LayerAssetFacts>> {
        let key = self.asset_value(asset)?;
        // The asset's details: its status. No asset, no facts.
        let Some(details) = self
            .fetch(at, &self.names.assets, "Asset", vec![key.clone()])
            .await?
        else {
            return Ok(None);
        };
        let details = details.to_value().map_err(invalid)?;
        let status = match field(&details, "status").and_then(variant_name) {
            Some("Live") => LayerAssetStatus::Live,
            Some("Frozen") => LayerAssetStatus::Frozen,
            Some("Destroying") => LayerAssetStatus::Destroying,
            other => return Err(invalid(format!("an asset status {other:?}"))),
        };
        // Its metadata. Storage absent is no metadata, never the type's default.
        let Some(metadata) = self
            .fetch(at, &self.names.assets, "Metadata", vec![key])
            .await?
        else {
            return Ok(None);
        };
        let metadata = metadata.to_value().map_err(invalid)?;
        let text = |name: &str| -> ChainResult<Vec<u8>> {
            let b = field(&metadata, name)
                .and_then(byte_string)
                .ok_or_else(|| invalid(format!("asset metadata without a {name}")))?;
            if b.len() > LAYER_TEXT_MAX {
                return Err(invalid(format!(
                    "an asset {name} longer than {LAYER_TEXT_MAX}"
                )));
            }
            Ok(b)
        };
        // Its own symbol and decimals fields; the name is never read (amendment 0011, `REQ-AS-8`).
        Ok(Some(LayerAssetFacts {
            symbol: text("symbol")?,
            decimals: field(&metadata, "decimals")
                .and_then(number)
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| invalid("asset metadata without decimals"))?,
            status,
        }))
    }

    async fn position(
        &self,
        at: &BlockRef,
        instance: u32,
        asset: &ChainAsset,
    ) -> ChainResult<u128> {
        let account = self.contract(instance)?;
        let key = self.asset_value(asset)?;
        let Some(t) = self
            .fetch(
                at,
                &self.names.assets,
                "Account",
                vec![key, account_value(&account)],
            )
            .await?
        else {
            return Ok(0);
        };
        let v = t.to_value().map_err(invalid)?;
        field(&v, "balance")
            .and_then(number)
            .ok_or_else(|| invalid("an asset account without a balance"))
    }

    async fn recorded_position(
        &self,
        at: &BlockRef,
        instance: u32,
        asset: &ChainAsset,
    ) -> ChainResult<u128> {
        let id = kreivo::parse_asset(&asset.0).map_err(invalid)?;
        self.read(
            at,
            instance,
            kreivo::message(kreivo::selector::POSITION, &id),
        )
        .await
    }

    fn venue(&self, instance: u32) -> Venue {
        match self.names.contract_of(instance) {
            Some(account) => Venue::Contract { account },
            // A deployment with no contract configured has no venue here; its anchors, if any,
            // are the pallet's.
            None => Venue::Pallet { instance },
        }
    }

    async fn native_balance(&self, at: &BlockRef, account: &AccountId32) -> ChainResult<u128> {
        let Some(t) = self
            .fetch(at, "System", "Account", vec![account_value(account)])
            .await?
        else {
            return Ok(0);
        };
        let v = t.to_value().map_err(invalid)?;
        field(&v, "data")
            .and_then(|d| field(d, "free"))
            .and_then(number)
            .ok_or_else(|| invalid("an account without a free balance"))
    }

    async fn account_nonce(&self, at: &BlockRef, account: &AccountId32) -> ChainResult<u64> {
        let Some(t) = self
            .fetch(at, "System", "Account", vec![account_value(account)])
            .await?
        else {
            return Ok(0);
        };
        let v = t.to_value().map_err(invalid)?;
        field(&v, "nonce")
            .and_then(number)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| invalid("an account without a nonce"))
    }

    /// The lesser of `System::BlockWeights`' normal-class maximum in the state of `at` and in that
    /// of its parent (`RpcChain::limit_in_state`). On Kreivo `0.17.0` the constant depends on
    /// where the block sits in its core (`MaxParachainBlockWeight`): a block that opens a core
    /// gives the whole core's (7 340 032 B of proof size on the suite's network), any other the
    /// fraction the pool holds a transaction to (2 446 677 B there, with one core and three
    /// blocks to it). Two blocks in a row never both open a core while a core carries more than
    /// one block, so the lesser is the fraction; with one block to a core the two are equal.
    /// Kept for the block it was read at, since one step sizes several batches against one
    /// finalized head.
    async fn extrinsic_limit(&self, at: &BlockRef) -> ChainResult<kreivo::Weight> {
        if let Some((hash, limit)) = *self
            .limit_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            if hash == at.hash {
                return Ok(limit);
            }
        }
        let here = self.limit_in_state(H256(at.hash)).await?;
        let header = match &self.dry_runs {
            Some(node) => node.current().chain_get_header(Some(H256(at.hash))).await,
            None => self.rpc.chain_get_header(Some(H256(at.hash))).await,
        }
        .map_err(unavailable)?
        .ok_or_else(|| unavailable(format!("no header for {}", hex::encode(at.hash))))?;
        let parent = self.limit_in_state(header.parent_hash).await?;
        let limit = here.min(parent);
        tracing::info!(
            at = at.number,
            proof_size = limit.proof_size,
            ref_time = limit.ref_time,
            here_proof_size = here.proof_size,
            parent_proof_size = parent.proof_size,
            "the runtime's extrinsic limit"
        );
        *self
            .limit_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((at.hash, limit));
        Ok(limit)
    }

    async fn dry_run(
        &self,
        at: &BlockRef,
        instance: u32,
        calls: &[BatchCall],
        origin: &AccountId32,
    ) -> ChainResult<DryRun> {
        let contract = self.contract(instance)?;
        let envelopes = calls
            .iter()
            .map(|c| kreivo::envelope(c, |i| self.names.contract_of(i)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(invalid)?;
        let args =
            kreivo::contracts_api_call_args(origin, &contract, &kreivo::execute_data(&envelopes));
        let bytes = match &self.dry_runs {
            Some(node) => node.call("ContractsApi_call", &args, H256(at.hash)).await?,
            None => {
                self.runtime_call("ContractsApi_call", &args, at.hash)
                    .await?
            }
        };
        kreivo::execute_dry_run(&bytes).map_err(invalid)
    }

    async fn sign_batch(
        &self,
        instance: u32,
        calls: &[BatchCall],
        signer: &dyn Signer,
        nonce: u64,
        era: &Mortality,
        context: &BlockRef,
        limits: &CallLimits,
    ) -> ChainResult<SignedExtrinsic> {
        // Born at the context, the finalized head: its hash is the birth hash, read from nothing.
        if era.birth != context.number {
            return Err(invalid(format!(
                "an era born at {} is signed only at its birth block, not at {}",
                era.birth, context.number
            )));
        }
        let contract = self.contract(instance)?;
        let envelopes = calls
            .iter()
            .map(|c| kreivo::envelope(c, |i| self.names.contract_of(i)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(invalid)?;
        let call = kreivo::contracts_call(&contract, limits, &kreivo::execute_data(&envelopes));
        let birth_hash = H256(context.hash);
        let version = self
            .rpc
            .state_get_runtime_version(None)
            .await
            .map_err(unavailable)?;
        let p = TxParams {
            genesis: self.client.genesis_hash().0,
            spec_version: version.spec_version,
            transaction_version: version.transaction_version,
            era: *era,
            birth_hash: birth_hash.0,
            nonce: u32::try_from(nonce).map_err(|_| invalid("a nonce past Kreivo's u32"))?,
            tip: 0,
        };
        let context = (
            u32::try_from(context.number).map_err(|_| invalid("a block past Kreivo's u32"))?,
            context.hash,
        );
        Ok(kreivo::sign(signer, &call, &p, context).await?)
    }

    /// Submit and keep watching until the client reports the transaction's end (finalized,
    /// dropped, invalid); while it is watched, the same bytes are not submitted again, since the
    /// client already broadcasts them. Watched again once that ends, as the adapter asks each
    /// step (`REQ-SO-7`).
    ///
    /// Never two watches of one transaction at a time: smoldot-light 0.17.2 panics when a
    /// transaction it holds has two status watchers and one is closed
    /// (`transactions_service.rs:1434`, `update_status`, "swap_remove index (is n) should be < len
    /// (is n)"). A submission that read one status and closed its watch (subxt's `submit`),
    /// repeated every step, did exactly that on the zombienet fleet, a few times an hour on
    /// Kreivo 0.17.0 (F-027 NOTES, decision 40).
    async fn submit(&self, extrinsic: &SignedExtrinsic) -> ChainResult<()> {
        let hash = extrinsic.hash;
        {
            let mut watching = self
                .watching
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            watching.retain(|_, w| !w.is_finished());
            if watching.contains_key(&hash) {
                return Ok(());
            }
        }
        let tx = SubmittableTransaction::from_bytes(self.client.clone(), extrinsic.bytes.clone());
        match tx.submit_and_watch().await {
            Ok(mut progress) => {
                let watch = tokio::spawn(async move {
                    // `NFR-15`: when the layer's client saw the transaction broadcast, in a best
                    // block and in a finalized one, from the moment it was submitted.
                    let submitted = std::time::Instant::now();
                    while let Some(status) = progress.next().await {
                        match status {
                            Ok(s) => {
                                let (name, block) = match &s {
                                    subxt::tx::TxStatus::Validated => ("validated", None),
                                    subxt::tx::TxStatus::Broadcasted => ("broadcast", None),
                                    subxt::tx::TxStatus::NoLongerInBestBlock => {
                                        ("no-longer-in-best-block", None)
                                    }
                                    subxt::tx::TxStatus::InBestBlock(b) => {
                                        ("in-best-block", Some(b.block_hash()))
                                    }
                                    subxt::tx::TxStatus::InFinalizedBlock(b) => {
                                        ("in-finalized-block", Some(b.block_hash()))
                                    }
                                    subxt::tx::TxStatus::Error { .. } => ("error", None),
                                    subxt::tx::TxStatus::Invalid { .. } => ("invalid", None),
                                    subxt::tx::TxStatus::Dropped { .. } => ("dropped", None),
                                };
                                tracing::info!(
                                    hash = %hex::encode(hash),
                                    status = name,
                                    block = %block.map(|b| hex::encode(b.0)).unwrap_or_default(),
                                    since_submit_ms = submitted.elapsed().as_millis() as u64,
                                    "a submitted transaction's status"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, hash = %hex::encode(hash), "a submitted transaction's watch ended");
                                break;
                            }
                        }
                    }
                });
                self.watching
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(hash, watch);
            }
            // Broadcast is repeated until the finalized head decides: a node that answers
            // "already imported", "stale" or "outdated" is not a failure of the adapter.
            Err(e) => {
                tracing::warn!(error = %e, hash = %hex::encode(hash), "a broadcast was not accepted")
            }
        }
        Ok(())
    }

    async fn find_extrinsic(
        &self,
        at: &BlockRef,
        from: u64,
        hash: &[u8; 32],
    ) -> ChainResult<Option<Included>> {
        // Back from `at` by parent hash: every block of the finalized chain down to `from`, with
        // no lookup by number (the light client answers none).
        let mut h = H256(at.hash);
        let mut n = at.number;
        while n >= from {
            let block = self.client.blocks().at(h).await.map_err(unavailable)?;
            let parent = block.header().parent_hash;
            let exts = block.extrinsics().await.map_err(unavailable)?;
            for ext in exts.iter() {
                if ext.hash().0 != *hash {
                    continue;
                }
                // Its events are state at that block, which a full node keeps for 256 blocks by
                // default (16 for the suite's pruned node): an adapter that was down longer than
                // that cannot read them. It is then not taken as a success: the inbox, read at the
                // finalized head, says what the batch did, and the core decides from the inbox and
                // the nonce, never from this.
                let events = match ext.events().await {
                    Ok(events) => events,
                    Err(e) => {
                        return Ok(Some(Included {
                            block: n,
                            success: false,
                            error: Some(format!(
                                "its events can no longer be read ({e}); the inbox decides"
                            )),
                        }));
                    }
                };
                let mut success = false;
                let mut error = None;
                for ev in events.iter().flatten() {
                    if ev.pallet_name() == "System" && ev.variant_name() == "ExtrinsicSuccess" {
                        success = true;
                    }
                    if ev.pallet_name() == "System" && ev.variant_name() == "ExtrinsicFailed" {
                        error = ev.field_values().ok().map(|c| {
                            Value::without_context(ValueDef::Composite(c.map_context(|_| ())))
                                .to_string()
                        });
                    }
                }
                return Ok(Some(Included {
                    block: n,
                    success,
                    error,
                }));
            }
            if n == 0 {
                break;
            }
            n -= 1;
            h = parent;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_asset_is_a_runtime_value_or_an_error() {
        assert_eq!(
            parse_asset(&ChainAsset("Here(9001)".into())),
            Ok(Value::unnamed_variant("Here", [Value::u128(9001)]))
        );
        assert!(parse_asset(&ChainAsset("Here(".into())).is_err());
        assert!(parse_asset(&ChainAsset("9001 trailing".into())).is_err());
    }

    #[test]
    fn deployments_map_both_ways() {
        let names = Names {
            contracts: "Contracts".into(),
            assets: "Assets".into(),
            deployments: vec![(1, [1; 32]), (2, [2; 32])],
        };
        assert_eq!(names.contract_of(2), Some([2; 32]));
        assert_eq!(names.instance_of(&[1; 32]), Some(1));
        assert_eq!(names.contract_of(3), None);
        assert_eq!(names.instance_of(&[3; 32]), None);
    }
}
