// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The network aledgent's end-to-end suite runs against: a relay chain and Kreivo.
//!
//! Adapted from an earlier zombienet topology (the same zombienet-sdk, the same collator flags,
//! the same release artifacts), with two differences:
//!
//! - **No Asset Hub.** That topology needed it for teleports. aledgent's contract moves a Kreivo
//!   asset between an account and the contract's position, so a third chain would only lengthen
//!   bring-up.
//! - **A second Kreivo node that prunes its state** (`kreivo-pruned`). It is not an authority. The
//!   suite reads through it to check what a pruned node can and cannot answer (T-027-06, "prune node
//!   state"): the contract's inbox at the finalized head, yes; a past block's events, no.
//!
//! Kreivo's genesis comes from the released runtime's own preset. zombienet validates the preset's
//! name against the list the runtime advertises (`development`, `local`), and the runtime refuses
//! `local` (it only answers to `local_testnet`), so what runs is `development`: balances, collators
//! and session keys, no communities and no assets.
//! The suite registers its own test community through the contracts store's flow, as a deployment
//! would. Assets cannot be created by a signed origin, so they are added here, as a genesis
//! override on a key the preset does not have (zombienet merges it; nothing else changes).

use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder, RegistrationStrategy};

/// Kreivo's para id, as its presets hard-code it.
pub const PARA_ID: u32 = 2281;
/// The Kreivo collator. The suite submits through it.
pub const COLLATOR: &str = "kreivo01";
/// A Kreivo full node that keeps only [`PRUNED_STATE_BLOCKS`] blocks of state.
pub const PRUNED_NODE: &str = "kreivo-pruned";
/// How many blocks of state `kreivo-pruned` keeps behind the finalized block.
pub const PRUNED_STATE_BLOCKS: u32 = 16;

/// A sufficient asset, minimum balance 1, held by Alice at genesis: `Here(1)`, as Kreivo's own
/// `local` preset creates it ("tUSD").
pub const SUFFICIENT_ASSET: u32 = 1;
/// A non-sufficient asset, minimum balance 1, held by Alice at genesis: `Here(2)`. The suite
/// checks whether it reaches the contract's account without an asset-account deposit (F-026
/// NOTES, "Not done: the real run").
pub const NON_SUFFICIENT_ASSET: u32 = 2;
/// A sufficient asset, minimum balance 1, held by Alice at genesis: `Here(3)`, symbol `BKO`. The
/// aledgent driver registers it book-only, moves it in the book, then binds it at zero divergence
/// (F-029, amendment 0011). Its name, `BKO/9`, disagrees with its decimals field on purpose: a
/// binding reads the symbol and decimals fields, never the name (`REQ-AS-8`).
pub const BOOK_ONLY_ASSET: u32 = 3;
/// The book-only asset's symbol, which is its id in the book.
pub const BOOK_ONLY_SYMBOL: &str = "BKO";
/// Every asset's decimals.
pub const ASSET_DECIMALS: u8 = 2;
/// What Alice holds of each asset at genesis.
pub const GENESIS_ASSET_BALANCE: u128 = 100_000_000_000;

/// An environment variable, or a default.
pub fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Alice's account, SS58 with the generic prefix, which Kreivo's genesis builder accepts.
pub fn alice() -> String {
    zombienet_sdk::subxt_signer::sr25519::dev::alice()
        .public_key()
        .to_account_id()
        .to_string()
}

/// The genesis override for Kreivo: the three assets, owned (issuer, admin and freezer) by Alice.
///
/// Being the asset's owner is what lets the suite freeze the contract's account and so make Kreivo
/// refuse a withdrawal (`Refusal::Kreivo`).
pub fn kreivo_genesis_overrides() -> serde_json::Value {
    let owner = alice();
    serde_json::json!({
        "assets": {
            "assets": [
                [{ "Here": SUFFICIENT_ASSET }, owner, true, 1],
                [{ "Here": NON_SUFFICIENT_ASSET }, owner, false, 1],
                [{ "Here": BOOK_ONLY_ASSET }, owner, true, 1],
            ],
            "accounts": [
                [{ "Here": SUFFICIENT_ASSET }, owner, GENESIS_ASSET_BALANCE],
                [{ "Here": NON_SUFFICIENT_ASSET }, owner, GENESIS_ASSET_BALANCE],
                [{ "Here": BOOK_ONLY_ASSET }, owner, GENESIS_ASSET_BALANCE],
            ],
            // Two decimals: the aledgent driver books `USD` and `BKO` at 2; `BKO`'s binding checks
            // its decimals field against that (F-029).
            "metadata": [
                [{ "Here": SUFFICIENT_ASSET }, b"testUSD".to_vec(), b"tUSD".to_vec(), ASSET_DECIMALS],
                [{ "Here": NON_SUFFICIENT_ASSET }, b"testEUR".to_vec(), b"tEUR".to_vec(), ASSET_DECIMALS],
                [{ "Here": BOOK_ONLY_ASSET }, b"BKO/9".to_vec(), BOOK_ONLY_SYMBOL.as_bytes().to_vec(), ASSET_DECIMALS],
            ],
        }
    })
}

/// The network configuration.
///
/// Inputs, from the environment (`e2e/scripts/prepare.sh` fetches them):
///
/// - `ZOMBIENET_KREIVO_WASM`: the released Kreivo runtime (`kreivo_runtime.compact.compressed.wasm`).
/// - `ZOMBIENET_POLKADOT_BIN`, `ZOMBIENET_POLKADOT_PARACHAIN_BIN`: polkadot-sdk release binaries.
/// - `ZOMBIENET_RELAY_CHAIN` (default `rococo-local`, built into the polkadot binary).
/// - `ZOMBIENET_KREIVO_PRESET` (optional): a preset name for the runtime.
pub fn network_config() -> NetworkConfig {
    let wasm = std::env::var("ZOMBIENET_KREIVO_WASM").expect(
        "ZOMBIENET_KREIVO_WASM must point at a released Kreivo runtime wasm; \
         run e2e/scripts/prepare.sh",
    );
    let preset = std::env::var("ZOMBIENET_KREIVO_PRESET").ok();
    let polkadot_bin = env_or("ZOMBIENET_POLKADOT_BIN", "polkadot");
    let parachain_bin = env_or("ZOMBIENET_POLKADOT_PARACHAIN_BIN", "polkadot-parachain");
    let relay_chain = env_or("ZOMBIENET_RELAY_CHAIN", "rococo-local");
    let pruning = PRUNED_STATE_BLOCKS.to_string();

    NetworkConfigBuilder::new()
        .with_relaychain(|r| {
            r.with_chain(relay_chain.as_str())
                .with_default_command(polkadot_bin.as_str())
                .with_validator(|n| n.with_name("alice"))
                .with_validator(|n| n.with_name("bob"))
        })
        .with_parachain(|p| {
            p.with_id(PARA_ID)
                .with_chain("kreivo-local")
                .with_chain_spec_runtime(wasm.as_str(), preset.as_deref())
                .with_genesis_overrides(kreivo_genesis_overrides())
                .cumulus_based(true)
                // Onboarded at relay genesis: the earlier suite found the post-launch registration
                // path left the para without a core and the collator idle at block 0.
                .with_registration_strategy(RegistrationStrategy::InGenesis)
                .with_default_command(parachain_bin.as_str())
                // Kreivo sets `RelayParentOffset = 1`, which needs the slot-based collator: the
                // default lookahead collator supplies no relay-parent descendant proofs and the
                // runtime panics every block with `InvalidNumberOfDescendants`.
                //
                // The collator also keeps all canonical state, so the suite can read any past
                // block's events from it however long the run takes: the contrast with
                // `kreivo-pruned` is then the point, not an accident of timing.
                .with_collator(|c| {
                    c.with_name(COLLATOR).with_args(vec![
                        "--force-authoring".into(),
                        ("--authoring", "slot-based").into(),
                        ("--state-pruning", "archive-canonical").into(),
                        // Why the pool drops a transaction it had taken: only in its debug log.
                        ("-l", "txpool=debug").into(),
                    ])
                })
                .with_fullnode(|n| {
                    n.with_name(PRUNED_NODE).with_args(vec![
                        ("--state-pruning", pruning.as_str()).into(),
                        ("--blocks-pruning", pruning.as_str()).into(),
                    ])
                })
        })
        .build()
        .unwrap_or_else(|errs| panic!("invalid zombienet config: {errs:?}"))
}
