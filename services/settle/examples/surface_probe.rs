// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A read-only probe of a Kreivo runtime's transaction surface (`T-027-12`, `REQ-SA-9`): what the
//! adapter reads from a live runtime, the format it would sign in, and the runtime's own verdict
//! on a transaction signed in it by **throwaway, unfunded keys**. Nothing is broadcast.
//!
//!   surface_probe (<rpc-url> | --light <kreivo-spec> <relay-spec>) [--contract <hex>] [--metadata <out>]
//!
//! It connects as the adapter does: to a node's RPC, or (`--light`) through its own light client
//! (`RpcChain::light`) with the chain specs the image bakes in (`deploy/templates/chain-specs/`),
//! which is how a deployment validates by default. It prints, for the runtime at the best head (`Chain::runtime`): its `spec_version`, its surface
//! (`transaction_version`, extensions in order, the types digest) and the surface's fingerprint,
//! and the format `TxFormat::by_surface` chooses. With `--metadata`, it also saves
//! `Metadata_metadata_at_version(15)`'s answer at that block, which `t_027_12_write_surface_fixtures`
//! (`src/chain/surface.rs`) reduces to `fixtures/surface-<spec>.json`.
//!
//! Then it signs one `Contracts::call` (a withdrawal to an account, to a contract address that need
//! not exist: a validation does not run the call) with the adapter's own encoder, as a native
//! account (`F-045`) and as a PASS device (`T-027-03`), each from a key made for the run, and has
//! the runtime validate it at that block (`TaggedTransactionQueue_validate_transaction`, through
//! `Chain::validate`), with three controls each:
//!
//! - **as signed**: a native transaction reaches fees (`Invalid(Payment)`, the format accepted); a
//!   PASS transaction from an unregistered device reaches its authentication (`BadSigner`), which
//!   shows it decodes and that `PassAuthenticate` ran, not that the message order is right (only a
//!   registered device shows that, which this probe has none of);
//! - **a signature byte flipped**: native `BadProof`;
//! - **signed with `transaction_version` − 1**: native `BadProof`, so the check sees the version.
//!
//! With `--contract` (a deployment's contract account, as instance 1), it also reads what a step
//! reads through `ContractsApi_call` at the finalized head (the registration, the chain's head,
//! the block's extrinsic limit) and dry-runs the deployment's next anchor as its last key, as a
//! batch's sizing does: read only, nothing signed.
//!
//! Not part of the adapter.

use std::sync::Arc;

use settle::chain::kreivo::{self, AssetId, Call, CallLimits, Destination, Envelope, Weight};
use settle::chain::rpc::{Names, RpcChain};
use settle::chain::{
    AccountId32, BatchCall, BatchCallKind, Chain, Mortality, SignedExtrinsic, blake2_256,
};
use settle::signer::{DevEd25519, DevSigner, PassAccount, Signer};

#[tokio::main]
async fn main() -> Result<(), String> {
    const USAGE: &str = "usage: surface_probe (<rpc-url> | --light <kreivo-spec> <relay-spec>) \
                         [--contract <hex>] [--metadata <out>]";
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let metadata_out = flag("--metadata");
    let contract: Option<AccountId32> = match flag("--contract") {
        Some(h) => Some(
            hex::decode(h.trim_start_matches("0x"))
                .map_err(d)?
                .try_into()
                .map_err(|_| "a contract account is 32 bytes")?,
        ),
        None => None,
    };
    let names = Names {
        contracts: "Contracts".into(),
        assets: "Assets".into(),
        deployments: contract.map(|c| vec![(1, c)]).unwrap_or_default(),
    };
    let chain = if args.first().map(String::as_str) == Some("--light") {
        let read = |i: usize| -> Result<String, String> {
            std::fs::read_to_string(args.get(i).ok_or(USAGE)?).map_err(d)
        };
        let (kreivo_spec, relay_spec) = (read(1)?, read(2)?);
        let chain = RpcChain::light(&kreivo_spec, Some(&relay_spec), names, &[])
            .await
            .map_err(d)?;
        println!("connection      light client");
        chain
    } else {
        let url = args.first().ok_or(USAGE)?;
        println!("connection      {url}");
        RpcChain::connect(url, names, &[]).await.map_err(d)?
    };
    let runtime = chain.runtime().await.map_err(d)?;
    let s = &runtime.surface;
    println!(
        "block           #{} 0x{}",
        runtime.at.number,
        hex::encode(runtime.at.hash)
    );
    println!("spec_version    {}", runtime.spec_version);
    println!("tx_version      {}", s.transaction_version);
    println!("extensions      {}", s.extensions.join(", "));
    println!(
        "types           {}",
        s.types.map(hex::encode).unwrap_or_else(|| "-".into())
    );
    println!("surface         {}", hex::encode(s.fingerprint()));
    let format = kreivo::TxFormat::by_surface(s);
    println!(
        "format          {}",
        format.map_or("none (unknown_surface)", |f| f.name)
    );
    if let Some(out) = metadata_out {
        let bytes = chain
            .runtime_call(
                "Metadata_metadata_at_version",
                &15u32.to_le_bytes(),
                runtime.at.hash,
            )
            .await
            .map_err(d)?;
        std::fs::write(&out, bytes).map_err(d)?;
        println!("metadata        {out}");
    }
    if contract.is_some() {
        read_deployment(&chain).await?;
    }
    let Some(format) = format else {
        return Ok(());
    };

    // The call, as the adapter builds a batch of one withdrawal.
    let seed = blake2_256(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(d)?
            .as_nanos()
            .to_le_bytes(),
    );
    let envelope = Envelope {
        operation_ref: blake2_256(&[&seed[..], b"op"].concat()),
        call: Call::Withdraw {
            to: Destination::Account(blake2_256(&[&seed[..], b"to"].concat())),
            asset: AssetId::Here(1),
            amount: 1,
        },
    };
    let limits = CallLimits {
        gas_limit: Weight {
            ref_time: 50_000_000_000,
            proof_size: 1_000_000,
        },
        storage_deposit_limit: Some(1_000_000_000_000),
    };
    let contract = blake2_256(&[&seed[..], b"contract"].concat());
    let call = kreivo::contracts_call(
        &contract,
        &limits,
        &kreivo::execute_data(std::slice::from_ref(&envelope)),
    );
    let fin = chain.finalized().await.map_err(d)?;
    let genesis = runtime.genesis;
    let params = |transaction_version: u32| kreivo::TxParams {
        genesis,
        spec_version: runtime.spec_version,
        transaction_version,
        era: Mortality::at(64, fin.number),
        birth_hash: fin.hash,
        nonce: 0,
        tip: 0,
    };
    let context = (u32::try_from(fin.number).map_err(d)?, fin.hash);
    let tv = s.transaction_version;

    let native = DevEd25519::from_secret(&blake2_256(&[&seed[..], b"native"].concat()));
    println!(
        "\nnative account  {} (throwaway, unfunded)",
        hex::encode(native.account())
    );
    let device: Arc<dyn Signer> =
        Arc::new(DevSigner::from_secret(&blake2_256(&[&seed[..], b"device"].concat())).map_err(d)?);
    let pass = PassAccount::new(device, blake2_256(&[&seed[..], b"user"].concat()));
    println!(
        "pass account    {} (throwaway, unregistered)",
        hex::encode(pass.account())
    );

    for (name, signer) in [
        ("native", &native as &dyn Signer),
        ("pass", &pass as &dyn Signer),
    ] {
        let signed = kreivo::sign(signer, &format, &call, &params(tv), context)
            .await
            .map_err(d)?;
        let flipped = flip_signature(signer, &signed)?;
        let older = kreivo::sign(signer, &format, &call, &params(tv - 1), context)
            .await
            .map_err(d)?;
        for (case, xt) in [
            (format!("tx {tv}"), &signed),
            (format!("tx {tv}, signature flipped"), &flipped),
            (format!("tx {}", tv - 1), &older),
        ] {
            let v = chain.validate(&runtime.at, xt).await.map_err(d)?;
            println!("{name:<7} {case:<28} {v:?}");
        }
    }
    Ok(())
}

/// What a step reads of instance 1's contract at the finalized head, and a dry run of its next
/// anchor as its last key: `ContractsApi_call` on this runtime, read only.
async fn read_deployment(chain: &RpcChain) -> Result<(), String> {
    let fin = chain.finalized().await.map_err(d)?;
    let instance = chain.instance(&fin, 1).await.map_err(d)?;
    println!("\nfinalized       #{}", fin.number);
    println!("instance 1      {instance:?}");
    println!(
        "chain_head      {:?}",
        chain.chain_head(&fin, 1).await.map_err(d)?
    );
    println!(
        "extrinsic_limit {:?}",
        chain.extrinsic_limit(&fin).await.map_err(d)?
    );
    let Some(view) = instance else {
        return Ok(());
    };
    let Some(key) = view.keys.last() else {
        return Ok(());
    };
    let anchor = BatchCall {
        reference: blake2_256(b"surface_probe/anchor"),
        kind: BatchCallKind::Anchor {
            number: view.last_anchor + 1,
            commitment: [0; 32],
        },
    };
    let dry = chain.dry_run(&fin, 1, &[anchor], key).await.map_err(d)?;
    println!(
        "dry run         anchor {} as {}: {dry:?}",
        view.last_anchor + 1,
        hex::encode(key)
    );
    Ok(())
}

/// An error, for the operator to read.
fn d<E: std::fmt::Debug>(e: E) -> String {
    format!("{e:?}")
}

/// `ext` with one byte of its signature flipped: the native transaction's `MultiSignature`, or the
/// PASS credential's device signature.
fn flip_signature(signer: &dyn Signer, ext: &SignedExtrinsic) -> Result<SignedExtrinsic, String> {
    let sig: Vec<u8> = if signer.pass_user().is_some() {
        kreivo::decode_extrinsic(&ext.bytes)?
            .credential
            .signature
            .to_vec()
    } else {
        kreivo::decode_native(&ext.bytes)?.signature.encode_multi()[1..].to_vec()
    };
    let at = ext
        .bytes
        .windows(sig.len())
        .position(|w| w == sig.as_slice())
        .ok_or("the signature is not in the transaction")?;
    let mut bytes = ext.bytes.clone();
    bytes[at + 10] ^= 1;
    Ok(SignedExtrinsic {
        hash: blake2_256(&bytes),
        bytes,
    })
}
