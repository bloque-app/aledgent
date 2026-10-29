// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `kreivo-pin` (`F-027` T-027-03): the settlement executor's Kreivo transaction, rebuilt from
//! Kreivo's own crates at the versions Kreivo `4aef055` (spec 133) locks, and checked byte for
//! byte against `vectors.json`, the file `services/settle` pins its own encoder to
//! (`t_027_03_the_pass_transaction_matches_the_pinned_vector` in `src/chain/kreivo.rs`).
//!
//! What comes from Kreivo's crates here, and what is read off Kreivo's source:
//!
//! - **`sp-runtime` 48.0.0**: the v5 general extrinsic's framing (`UncheckedExtrinsic`, decoded
//!   and re-encoded), the mortal era (`Era`), `MultiAddress`, `MultiSignature` and its
//!   verification, and the inherited implication `PassAuthenticate` hashes (`ImplicationParts`,
//!   `TxBaseImplication`).
//! - **`pass-authenticators-substrate-keys` 1.0.0**: the message the device signs
//!   (`SignedMessage::message`, whose order is `context ‖ challenge ‖ authority` at this version),
//!   the credential (`KeySignature`), and the device's own verification of it
//!   (`Device::verify_user`: the authority, the challenge through the challenger, the signature).
//! - **`fc-traits-authn` 2.2.0**: the authority (`AuthorityFromPalletId`), the challenger trait.
//! - **`sp-weights` 36.0.0**: the gas limit's `Weight`.
//! - **Read off Kreivo's source at `4aef055`**, because no crate exports them without a runtime:
//!   the order of the transaction extensions (`runtime/kreivo/src/lib.rs`, `TransactionExtensions`,
//!   with `DynamicMaxBlockWeight`, `SkipCheckIfFeeless` and `ChargeGasTxPayment` encoding as
//!   what they wrap); `Contracts` at pallet index 80 and `call` at call index 6 (`pallet-contracts`
//!   49.0.0); the `Pass` composite's credential enum, `WebAuthn` then `SubstrateKey`
//!   (`config/system.rs`); `fc-pallet-pass` 2.2.0's `PassAuthenticate(Option<AuthenticateParams {
//!   device_id, credential }>)` and its address generator; `pallet-asset-tx-payment` 49.0.0's
//!   `ChargeAssetTxPayment { #[codec(compact)] tip, asset_id: Option<_> }`; and Kreivo's
//!   `BlockHashChallenger`, copied below.
//!
//! **`F-045`: the native transaction** (`vectors-native.json`, pinned by
//! `f_045_the_native_transaction_matches_the_pinned_vector` in `src/chain/kreivo.rs`): the same
//! `UncheckedExtrinsic`, in its v4 signed form, from a native account (`MultiAddress::Id`) with a
//! `MultiSignature::Ed25519`, `PassAuthenticate` carrying `None`; and the signature checked over the
//! payload `UncheckedExtrinsic::check` builds (`sp-runtime` 48.0.0: `call ‖ explicit ‖ implicit`,
//! hashed past 256 bytes), with `sp-core`'s Ed25519.

#[cfg(test)]
mod tests {
    use codec::{Compact, Decode, DecodeWithMemTracking, Encode};
    use fc_traits_authn::util::AuthorityFromPalletId;
    use fc_traits_authn::{Challenge, Challenger, ExtrinsicContext, UserAuthenticator};
    use frame_support::traits::Get;
    use frame_support::PalletId;
    use pass_authenticators_substrate_keys::{KeySignature, SignedMessage};
    use serde_json::Value;
    use sp_core::crypto::AccountId32;
    use sp_core::{Pair, H256};
    use sp_crypto_hashing::blake2_256;
    use sp_runtime::generic::{Era, Preamble, UncheckedExtrinsic};
    use sp_runtime::traits::{
        ExtensionVariant, IdentifyAccount, ImplicationParts, TxBaseImplication, Verify,
    };
    use sp_runtime::{MultiAddress, MultiSignature};
    use sp_weights::Weight;

    frame_support::parameter_types! {
        /// Kreivo's `PassPalletId` (`runtime/kreivo/src/config/system.rs`).
        pub PassPalletId: PalletId = PalletId(*b"kreivo_p");
    }

    fn vector() -> Value {
        serde_json::from_str(include_str!("../vectors.json")).expect("vectors.json")
    }

    fn bytes(v: &Value) -> Vec<u8> {
        hex::decode(v.as_str().expect("hex")).expect("hex")
    }

    fn arr32(v: &Value) -> [u8; 32] {
        bytes(v).try_into().expect("32 bytes")
    }

    fn num(v: &Value) -> u64 {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .expect("a number")
    }

    /// The block the vector's challenge context names, and no other: Kreivo's
    /// `System::block_hash`, as far as this test needs it.
    fn block_hash(cx: u32) -> [u8; 32] {
        let v = vector();
        if u64::from(cx) == num(&v["inputs"]["context_number"]) {
            arr32(&v["inputs"]["context_hash"])
        } else {
            [0; 32]
        }
    }

    /// How far the chain is when the transaction is validated in this test.
    const NOW: u32 = 1_000;
    /// `30 * MINUTES` of Kreivo's blocks, at the slowest block rate.
    const PAST_BLOCKS: u32 = 300;

    /// The vector's device key: `services/settle`'s test secret (`DevSigner::from_secret`).
    const DEVICE_SECRET: [u8; 32] = [0x5e; 32];

    /// Kreivo's `BlockHashChallenger` (`runtime/kreivo/src/config/system.rs` at `4aef055`),
    /// over [`block_hash`] and a chain at block [`NOW`].
    struct KreivoChallenger;

    impl Challenger for KreivoChallenger {
        type Context = u32;

        fn generate(cx: &Self::Context, xtc: &impl ExtrinsicContext) -> Challenge {
            blake2_256(&[&block_hash(*cx)[..], xtc.as_ref()].concat())
        }

        fn check_challenge(
            cx: &Self::Context,
            xtc: &impl ExtrinsicContext,
            challenge: &[u8],
        ) -> Option<()> {
            (*cx >= NOW.saturating_sub(PAST_BLOCKS)).then_some(())?;
            (Self::generate(cx, xtc)[..] == *challenge).then_some(())
        }
    }

    type Device = pass_authenticators_substrate_keys::Device<
        KreivoChallenger,
        AuthorityFromPalletId<PassPalletId>,
    >;

    /// Kreivo's `Pass` composite credential (`composite_authenticator!(pub Pass<…> { WebAuthn,
    /// SubstrateKey })`): the variant index is the authenticator's place in that list.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking)]
    enum PassCredential {
        #[codec(index = 0)]
        WebAuthn(()),
        #[codec(index = 1)]
        SubstrateKey(KeySignature<u32>),
    }

    /// `fc_pallet_pass::AuthenticateParams`.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking)]
    struct AuthenticateParams {
        device_id: [u8; 32],
        credential: PassCredential,
    }

    /// `fc_pallet_pass::PassAuthenticate`.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking)]
    struct PassAuthenticate(Option<AuthenticateParams>);

    /// `pallet_asset_tx_payment::ChargeAssetTxPayment`, inside `SkipCheckIfFeeless` and
    /// `ChargeGasTxPayment`.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking)]
    struct ChargeAssetTxPayment {
        #[codec(compact)]
        tip: u128,
        asset_id: Option<()>,
    }

    /// Kreivo's `TransactionExtensions`' explicit data, in order: `PassAuthenticate`,
    /// `CheckNonZeroSender`, `CheckSpecVersion`, `CheckTxVersion`, `CheckGenesis`, `CheckEra`,
    /// `CheckNonce` (compact, `Nonce = u32`), `CheckWeight`, `SkipCheckIfFeeless<…>`.
    type Extensions = (
        PassAuthenticate,
        (),
        (),
        (),
        (),
        Era,
        Compact<u32>,
        (),
        ChargeAssetTxPayment,
    );

    /// A `RuntimeCall` as its bytes: it encodes as them, and decodes as the rest of the input.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RawCall(Vec<u8>);

    impl Encode for RawCall {
        fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
            dest.write(&self.0);
        }
    }

    impl Decode for RawCall {
        fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
            let n = input.remaining_len()?.ok_or("no length")?;
            let mut out = vec![0u8; n];
            input.read(&mut out)?;
            Ok(RawCall(out))
        }
    }

    impl DecodeWithMemTracking for RawCall {}

    /// Kreivo's `UncheckedExtrinsic`, with the call as bytes.
    type Extrinsic =
        UncheckedExtrinsic<MultiAddress<AccountId32, u16>, RawCall, MultiSignature, Extensions>;

    fn native_vector() -> Value {
        serde_json::from_str(include_str!("../vectors-native.json")).expect("vectors-native.json")
    }

    /// `F-045`: a native account's transaction, as Kreivo's crates decode and check it, equals the
    /// pinned vector: v4 signed, from the key's own account, `PassAuthenticate` with no credential,
    /// and an Ed25519 signature over `SignedPayload`'s bytes that `MultiSignature::verify` accepts
    /// for that account and for no other payload.
    #[test]
    fn f_045_the_pinned_native_transaction_is_kreivos() {
        let v = native_vector();
        let (i, o) = (&v["inputs"], &v["outputs"]);
        // The account is the key: `MultiSigner::Ed25519(public).into_account()`.
        let public = sp_core::ed25519::Public::from_raw(arr32(&i["public"]));
        let account: AccountId32 = sp_runtime::MultiSigner::Ed25519(public).into_account();
        assert_eq!(
            account,
            AccountId32::from(arr32(&o["account"])),
            "the account"
        );
        // The key is `services/settle`'s test seed (`DevEd25519::from_secret`).
        let key = sp_core::ed25519::Pair::from_seed(&DEVICE_SECRET);
        assert_eq!(key.public(), public, "the vector's key");
        let current = num(&i["era_current"]);
        let era = Era::mortal(num(&i["era_period"]), current);
        assert_eq!(era.encode(), bytes(&o["era"]), "the era");
        let contract = arr32(&i["contract"]);
        let sdl: Option<u128> = i["storage_deposit_limit"]
            .as_str()
            .map(|s| s.parse().expect("a u128"));
        let call = (
            80u8,
            6u8,
            MultiAddress::<AccountId32, u16>::Id(AccountId32::from(contract)),
            Compact(0u128),
            Weight::from_parts(num(&i["gas_ref_time"]), num(&i["gas_proof_size"])),
            sdl.map(Compact),
            bytes(&i["execute_data"]),
        )
            .encode();
        assert_eq!(call, bytes(&o["call"]), "the call");
        let nonce = u32::try_from(num(&i["nonce"])).expect("Kreivo's nonce is u32");
        let tip: u128 = i["tip"].as_str().unwrap().parse().unwrap();
        let explicit: Extensions = (
            PassAuthenticate(None),
            (),
            (),
            (),
            (),
            era,
            Compact(nonce),
            (),
            ChargeAssetTxPayment {
                tip,
                asset_id: None,
            },
        );
        assert_eq!(
            explicit.encode(),
            bytes(&o["explicit"]),
            "the explicit data"
        );
        // Every extension's implicit data, `PassAuthenticate`'s `()` first.
        let implicit = (
            (),
            (),
            u32::try_from(num(&i["spec_version"])).unwrap(),
            u32::try_from(num(&i["transaction_version"])).unwrap(),
            H256(arr32(&i["genesis"])),
            H256(arr32(&i["birth_hash"])),
            (),
            (),
            (),
        );
        assert_eq!(
            implicit.encode(),
            bytes(&o["implicit"]),
            "the implicit data"
        );
        // The transaction, as Kreivo decodes it, and re-encodes to the same bytes.
        let raw = bytes(&o["extrinsic"]);
        let xt = Extrinsic::decode(&mut &raw[..]).expect("Kreivo decodes the transaction");
        assert_eq!(xt.encode(), raw);
        assert_eq!(blake2_256(&raw).to_vec(), bytes(&o["extrinsic_hash"]));
        let Preamble::Signed(address, signature, ext) = &xt.preamble else {
            panic!("not a v4 signed transaction: {:?}", xt.preamble)
        };
        assert_eq!(*address, MultiAddress::Id(account.clone()));
        assert_eq!(*ext, explicit);
        assert_eq!(xt.function, RawCall(call.clone()));
        let MultiSignature::Ed25519(sig) = signature else {
            panic!("not an Ed25519 signature: {signature:?}")
        };
        assert_eq!(sig.0.to_vec(), bytes(&o["signature"]), "the signature");
        // `UncheckedExtrinsic::check`'s payload: `(call, explicit, implicit)` encoded, its
        // blake2b-256 past 256 bytes; and the signature verifies over it for this account.
        let payload = (RawCall(call.clone()), explicit.clone(), implicit).encode();
        let payload = if payload.len() > 256 {
            blake2_256(&payload).to_vec()
        } else {
            payload
        };
        assert_eq!(payload, bytes(&o["signed_payload"]), "the signed payload");
        assert!(
            signature.verify(&payload[..], &account),
            "the signature verifies"
        );
        // Not over another payload, nor for another account.
        let mut other = payload.clone();
        other[0] ^= 1;
        assert!(!signature.verify(&other[..], &account));
        assert!(!signature.verify(&payload[..], &AccountId32::from([0x11; 32])));
    }

    /// `T-027-03`: the transaction, its call, its extensions, the challenge and the device's
    /// signature, as Kreivo's crates build and check them, equal the pinned vector.
    #[test]
    fn t_027_03_the_pinned_transaction_is_kreivos() {
        let v = vector();
        let (i, o) = (&v["inputs"], &v["outputs"]);
        // The era: `CheckEra`'s explicit data, and the birth block whose hash it binds.
        let current = num(&i["era_current"]);
        let era = Era::mortal(num(&i["era_period"]), current);
        assert_eq!(era.encode(), bytes(&o["era"]), "the era");
        assert_eq!(era.birth(current), num(&o["era_birth"]), "the era's birth");
        // The call: `Contracts::call { dest, value, gas_limit, storage_deposit_limit, data }`.
        let contract = arr32(&i["contract"]);
        let sdl: Option<u128> = i["storage_deposit_limit"]
            .as_str()
            .map(|s| s.parse().expect("a u128"));
        let call = (
            80u8,
            6u8,
            MultiAddress::<AccountId32, u16>::Id(AccountId32::from(contract)),
            Compact(0u128),
            Weight::from_parts(num(&i["gas_ref_time"]), num(&i["gas_proof_size"])),
            sdl.map(Compact),
            bytes(&i["execute_data"]),
        )
            .encode();
        assert_eq!(call, bytes(&o["call"]), "the call");
        // Every extension after PassAuthenticate: its explicit and its implicit data.
        let nonce = u32::try_from(num(&i["nonce"])).expect("Kreivo's nonce is u32");
        let tip: u128 = i["tip"].as_str().unwrap().parse().unwrap();
        let explicit = (
            (),
            (),
            (),
            (),
            era,
            Compact(nonce),
            (),
            ChargeAssetTxPayment {
                tip,
                asset_id: None,
            },
        );
        assert_eq!(explicit.encode(), bytes(&o["explicit_after_pass"]));
        let implicit = (
            (),
            u32::try_from(num(&i["spec_version"])).unwrap(),
            u32::try_from(num(&i["transaction_version"])).unwrap(),
            H256(arr32(&i["genesis"])),
            H256(arr32(&i["birth_hash"])),
            (),
            (),
            (),
        );
        assert_eq!(implicit.encode(), bytes(&o["implicit_after_pass"]));
        // The inherited implication PassAuthenticate hashes: the base (extension version 0 and
        // the call), then the rest's explicit and implicit data.
        let implication = ImplicationParts {
            base: TxBaseImplication((0u8, RawCall(call.clone()))),
            explicit,
            implicit,
        };
        let xtc = blake2_256(&implication.encode());
        assert_eq!(
            xtc.to_vec(),
            bytes(&o["extrinsic_context"]),
            "the extrinsic context"
        );
        // Kreivo's challenger, then the message pass-authenticators 1.0.0 has the device sign.
        let context = u32::try_from(num(&i["context_number"])).unwrap();
        let challenge = KreivoChallenger::generate(&context, &xtc);
        assert_eq!(challenge.to_vec(), bytes(&o["challenge"]), "the challenge");
        let message = SignedMessage {
            context,
            challenge,
            authority_id: AuthorityFromPalletId::<PassPalletId>::get(),
        };
        assert_eq!(
            message.message().as_ref(),
            &bytes(&o["signed_message"])[..],
            "the signed message: context, challenge, authority"
        );
        // The device's id is the account of its key; its signature verifies as Kreivo verifies
        // a `MultiSignature::Ecdsa`.
        let public = bytes(&i["device_public"]);
        let device_id = blake2_256(&public);
        assert_eq!(device_id.to_vec(), bytes(&o["device_id"]));
        let raw: [u8; 65] = bytes(&o["signature"]).try_into().unwrap();
        let signature = MultiSignature::Ecdsa(sp_core::ecdsa::Signature::from_raw(raw));
        assert!(signature.verify(message.message().as_ref(), &AccountId32::from(device_id)));
        // The credential, verified by the substrate-keys device itself: the authority, the
        // challenge through the challenger, the signature.
        let user_id = arr32(&i["user_id"]);
        let credential = KeySignature {
            user_id,
            message: message.clone(),
            signature: signature.clone(),
        };
        let mut device = Device::new(AccountId32::from(device_id));
        assert_eq!(device.verify_user(&credential, &xtc), Some(()));
        // T-027-07: the same device, key and challenge, signing spec 132's order (`context ‖
        // authority ‖ challenge`, what Kreivo 0.16.9 verifies) instead: this runtime refuses it.
        let key = sp_core::ecdsa::Pair::from_seed(&DEVICE_SECRET);
        assert_eq!(key.public().0.to_vec(), public, "the vector's device key");
        let other_order = [
            &context.to_le_bytes()[..],
            &message.authority_id[..],
            &challenge[..],
        ]
        .concat();
        let wrong = KeySignature {
            user_id,
            message: message.clone(),
            signature: MultiSignature::Ecdsa(key.sign(&other_order)),
        };
        assert_eq!(device.verify_user(&wrong, &xtc), None);
        // Bound to the call: another call is another context, and fails.
        let mut other = call.clone();
        let last = other.len() - 1;
        other[last] ^= 1;
        let other_xtc = blake2_256(
            &ImplicationParts {
                base: TxBaseImplication((0u8, RawCall(other))),
                explicit: implication.explicit.clone(),
                implicit: implication.implicit,
            }
            .encode(),
        );
        assert_eq!(device.verify_user(&credential, &other_xtc), None);
        // PassAuthenticate's value.
        let pass = PassAuthenticate(Some(AuthenticateParams {
            device_id,
            credential: PassCredential::SubstrateKey(credential),
        }));
        assert_eq!(
            pass.encode(),
            bytes(&o["pass_authenticate"]),
            "PassAuthenticate"
        );
        // The PASS account: `fc-pallet-pass`'s address generator for `()`, `BlakeTwo256` of
        // `[0; 32] ‖ user`.
        let mut input = [0u8; 64];
        input[32..].copy_from_slice(&user_id);
        assert_eq!(blake2_256(&input).to_vec(), bytes(&o["pass_account"]));
        // The whole transaction, as Kreivo decodes it: v5, general, extension version 0, these
        // extensions, this call; and it re-encodes to the same bytes.
        let raw = bytes(&o["extrinsic"]);
        let xt = Extrinsic::decode(&mut &raw[..]).expect("Kreivo decodes the transaction");
        let Preamble::General(ExtensionVariant::V0(ext)) = &xt.preamble else {
            panic!("not a v5 general transaction: {:?}", xt.preamble)
        };
        assert_eq!(ext.0, pass);
        assert_eq!(ext.5, era);
        assert_eq!(ext.6, Compact(nonce));
        assert_eq!(xt.function, RawCall(call));
        assert_eq!(xt.encode(), raw);
        assert_eq!(blake2_256(&raw).to_vec(), bytes(&o["extrinsic_hash"]));
    }
}
