// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `kreivo-pin-132` (`F-027` T-027-07): the settlement executor's spec 132 Kreivo transaction,
//! rebuilt from Kreivo's own crates at the versions Kreivo release `0.16.9` (`db3d8af`, spec 132,
//! the runtime live on Kreivo mainnet) locks, and checked byte for byte against `vectors.json`,
//! the file `services/settle` pins its own encoder to
//! (`t_027_07_the_spec_132_transaction_matches_its_pinned_vector` in `src/chain/kreivo.rs`).
//! `../kreivo-pin` does the same for spec 133 at Kreivo `4aef055`.
//!
//! What comes from Kreivo's crates here, and what is read off Kreivo's source:
//!
//! - **`sp-runtime` 44.0.0**: the v5 general extrinsic's framing (`UncheckedExtrinsic`, decoded
//!   and re-encoded), the mortal era (`Era`), `MultiAddress`, `MultiSignature` and its
//!   verification, and the inherited implication `PassAuthenticate` hashes (`ImplicationParts`,
//!   `TxBaseImplication`).
//! - **`pass-authenticators-substrate-keys` at `92c3484`**: the message the device signs
//!   (`SignedMessage::message`, whose order is **`context ‖ authority ‖ challenge`** at this
//!   revision, before pass-authenticators PR #16 moved the authority last), the credential
//!   (`KeySignature`), and the device's own verification of it (`Device::verify_user`: the
//!   authority, the challenge through the challenger, the signature).
//! - **`fc-traits-authn` 0.1.0 at frame-contrib `26630fb`**: the authority
//!   (`AuthorityFromPalletId`), the challenger trait.
//! - **`sp-weights` 33.1.0**: the gas limit's `Weight`.
//! - **Read off Kreivo's source at `0.16.9`**, because no crate exports them without a runtime:
//!   the order of the transaction extensions (`runtime/kreivo/src/lib.rs`, `TransactionExtensions`,
//!   not wrapped in `DynamicMaxBlockWeight` at this release, with `SkipCheckIfFeeless` and
//!   `ChargeGasTxPayment` encoding as what they wrap; live mainnet's metadata names the same nine);
//!   `Contracts` at pallet index 80 and `call` at call index 6 (`pallet-contracts` 43.0.0); the
//!   `Pass` composite's credential enum, `WebAuthn` then `SubstrateKey` (`config/system.rs`);
//!   `fc-pallet-pass` 1.0.0's `PassAuthenticate(Option<AuthenticateParams { device_id, credential
//!   }>)` and its address generator; `pallet-asset-tx-payment` 43.0.0's `ChargeAssetTxPayment {
//!   #[codec(compact)] tip, asset_id: Option<_> }`; and Kreivo's `BlockHashChallenger`, copied
//!   below.

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
    use sp_runtime::traits::{ImplicationParts, TxBaseImplication, Verify};
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
    /// Less than `30 * MINUTES` of Kreivo's blocks (3 600 at `0.16.9`'s 500 ms): the window the
    /// adapter's own `challenge_window` (256 by default) stays within.
    const PAST_BLOCKS: u32 = 300;

    /// The vector's device key: `services/settle`'s test secret (`DevSigner::from_secret`).
    const DEVICE_SECRET: [u8; 32] = [0x5e; 32];

    /// Kreivo's `BlockHashChallenger` (`runtime/kreivo/src/config/system.rs` at `0.16.9`),
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

    /// `T-027-07`: the lock file still pins the two git dependencies to Kreivo `0.16.9`'s
    /// revisions (they are declared without one, as Kreivo declares them).
    #[test]
    fn t_027_07_the_lock_pins_kreivo_0_16_9s_revisions() {
        let lock = include_str!("../Cargo.lock");
        for (name, source) in [
            (
                "pass-authenticators-substrate-keys",
                "git+https://github.com/virto-network/pass-authenticators#92c34842f069c85851341fcce3eed594c70cd834",
            ),
            (
                "fc-traits-authn",
                "git+https://github.com/virto-network/frame-contrib#26630fbe817de8e537b37a409cce444a1c14dfbd",
            ),
        ] {
            let entry = format!("name = \"{name}\"\nversion = \"0.1.0\"\nsource = \"{source}\"");
            assert!(lock.contains(&entry), "{name} is not locked at {source}");
        }
    }

    /// `T-027-07`: the spec 132 transaction, its call, its extensions, the challenge and the
    /// device's signature, as Kreivo 0.16.9's crates build and check them, equal the pinned
    /// vector.
    #[test]
    fn t_027_07_the_pinned_spec_132_transaction_is_kreivo_0_16_9s() {
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
        // Kreivo's challenger, then the message pass-authenticators `92c3484` has the device sign.
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
            "the signed message: context, authority, challenge"
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
        // The same device, key and challenge, signing spec 133's order (`context ‖ challenge ‖
        // authority`) instead: this runtime refuses it, as it refuses a credential with
        // `BadSigner`. The order is the whole difference between the two formats.
        let key = sp_core::ecdsa::Pair::from_seed(&DEVICE_SECRET);
        assert_eq!(key.public().0.to_vec(), public, "the vector's device key");
        let other_order = [
            &context.to_le_bytes()[..],
            &challenge[..],
            &message.authority_id[..],
        ]
        .concat();
        let wrong = KeySignature {
            user_id,
            message: message.clone(),
            signature: MultiSignature::Ecdsa(key.sign(&other_order)),
        };
        assert_eq!(device.verify_user(&wrong, &xtc), None);
        assert_eq!(
            MultiSignature::Ecdsa(key.sign(message.message().as_ref())),
            signature,
            "ECDSA over blake2_256 is deterministic: the vector's signature is this key's"
        );
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
        // sp-runtime 44 carries the extension version beside the extensions.
        let Preamble::General(0, ext) = &xt.preamble else {
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
