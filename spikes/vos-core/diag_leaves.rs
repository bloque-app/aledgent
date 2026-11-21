// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Shared by the guest (feature `diag-leaves`) and the client (`--leaves`): genesis's service
// leaves, recomputed from a genesis envelope and its blobs as `core_binding::transition` founds
// the book, as (kind, key, value) encoded with postcard. A diagnostic of a service-root divergence.
pub fn genesis_leaves(
    message: &[u8],
    fetch: &dyn Fn(&core_binding::envelope::BlobKey) -> Option<alloc::vec::Vec<u8>>,
) -> alloc::vec::Vec<u8> {
    use core_binding::envelope::{Envelope, Private, decode, encode};
    let Ok(Envelope::Genesis(g)) = decode::<Envelope>(message) else {
        return alloc::vec::Vec::new();
    };
    let private: Private = decode(&fetch(&g.private).expect("private")).expect("private");
    let witness =
        core_binding::witness::KernelWitness::from_bytes(&fetch(&g.witness).expect("witness"))
            .expect("witness");
    let store = core_binding::witness::WitnessStore::new(
        witness,
        core_binding::transition::empty_subroots(),
        None,
        0,
    )
    .expect("store");
    let vault = core_binding::vault::PrivateVault::new(None, alloc::collections::BTreeMap::new());
    let book = ledger_kernel::book::Book::bootstrap(store, vault, private.key_ring())
        .expect("bootstrap");
    let (ledger, _) =
        core_binding::ledger_core::Ledger::genesis(book, g.genesis, g.op, g.at).expect("genesis");
    encode(&ledger.state().all_leaves())
}
