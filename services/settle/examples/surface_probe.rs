// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A read-only probe of a Kreivo runtime's transaction surface (`T-027-12`, `REQ-SA-9`): what the
//! adapter reads from a live runtime, the format it would sign in, and the runtime's own verdict
//! on a transaction signed in it by **throwaway, unfunded keys**. Nothing is broadcast.
//!
//!   surface_probe <rpc-url> [metadata-out]
//!
//! It prints, for the runtime at the best head (`Chain::runtime`): its `spec_version`, its surface
//! (`transaction_version`, extensions in order, the types digest) and the surface's fingerprint,
//! and the format `TxFormat::by_surface` chooses. With `metadata-out`, it also saves
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
//! Not part of the adapter.

use std::sync::Arc;

use settle::chain::kreivo::{self, AssetId, Call, CallLimits, Destination, Envelope, Weight};
use settle::chain::rpc::{Names, RpcChain};
use settle::chain::{Chain, Mortality, SignedExtrinsic, blake2_256};
use settle::signer::{DevEd25519, DevSigner, PassAccount, Signer};

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .ok_or("usage: surface_probe <rpc-url> [metadata-out]")?;
    let metadata_out = args.next();
    let names = Names {
        contracts: "Contracts".into(),
        assets: "Assets".into(),
        deployments: Vec::new(),
    };
    let chain = RpcChain::connect(&url, names, &[]).await.map_err(d)?;
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
