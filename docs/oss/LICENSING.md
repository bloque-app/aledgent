<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Licensing

This document records which licence each part of aledgent carries, and why. It describes the
public tree that the branch `oss/public-tree` prepares. It is not legal advice. Before the first
public release, the owner should have counsel confirm §5 (obligations) and §6 (open points).

## 1. Summary

| Component | Paths | Licence | Why |
|---|---|---|---|
| The VOS side: the core Agent, its host, the shard Agent, aledgent's AgentRuntime | `agents/**` | **AGPL-3.0-or-later** | It links VOS, which is AGPL-3.0-or-later. There is no other choice. |
| The core library, the kernel binding, the leaf and audit stores, the verifier, the conformance suite, authentication, the shards prototype | `crates/**` | **Apache-2.0 OR MIT** | It does not link VOS. Integrators, auditors and other implementers should be able to embed the verifier and the conformance suite with no copyleft obligation. |
| Services: ingress, the settlement adapter, recovery, operations | `services/**`, `ops/**` | **Apache-2.0 OR MIT** | It reaches the core over a socket, not by linking (§4.2). |
| The settlement contract (source) | `contracts/settlement/**` | **Apache-2.0 OR MIT** (source) | The *built* contract is GPL-3.0-only: see §4.3. |
| End-to-end suite, stress driver, sidecar, scripts, CI gates, deployment templates | `e2e/**`, `tools/**`, `deploy/**`, `.github/**` | **Apache-2.0 OR MIT** | Tooling; never linked into a shipped binary that carries a copyleft licence. |
| Specification, plan, amendments, feature plans, roadmap, every Markdown or HTML document | `*.md`, `*.html` | **CC-BY-4.0** | The specification is meant to be implemented by others; CC-BY-4.0 lets anyone do so, with attribution. |
| Measurement code and data | `spikes/` code; `spikes/guest/**`, `spikes/vos-host/**`, `spikes/vos-external/**`; `spikes/results/**` | **Apache-2.0 OR MIT**; **AGPL-3.0-or-later** where it links VOS; **CC-BY-4.0** for the results | Measurement guests and hosts link VOS; results are data, not code. |
| Upstream patches | `proposals/cipherclerk/*.patch` | **Apache-2.0 OR MIT** | They modify CipherClerk, which is Apache-2.0 OR MIT; a patch carries the licence of what it patches. |
| Patches to VOS-linked code | `spikes/vos-external/*.patch`, `.github/x86-bench/core-host-backends.patch` | **AGPL-3.0-or-later** | Each carries the licence of what it patches (VOS-linked code). |
| aledgent's marks | `docs/brand/*.svg`, `site/src/assets/*.svg`, `site/public/favicon.svg` | **LicenseRef-Trademark-Marks** (all rights reserved) | The name and the marks are Pagos Cube LLC's trademarks, not licensed with the code (`docs/brand/README.md`). |
| Kreivo's chain specification | `deploy/templates/chain-specs/kreivo.json` | **GPL-3.0-only** (Kreivo's) | Published by Kreivo, kept as it is. |
| Third-party files kept verbatim | `services/ingress/certs/rds-global-bundle.pem` | Their publishers' terms (`REUSE.toml`) | The public CA bundle of the database provider. |

Every file says which of these it carries: a `SPDX-License-Identifier` header where the format
allows a comment, `REUSE.toml` otherwise. `LICENSES/` holds each licence's text, and the tree is
[REUSE](https://reuse.software)-compliant.

## 2. The recommendation: a fully open product, with copyleft only where it is forced

**Recommended: fully open, not open core.** Publish every component that makes up the product, the
core, ingress, the settlement adapter, the contract, the conformance suite, operations, and the
specification. Keep private only what describes *Bloque's own estate*: its migration, its
production deployment, its incident history and its commercial plans (`docs/oss/PRUNING-REPORT.md`
lists them). Those are not part of the product. They are the record of one operator.

The reasons:

1. **The product's value to an outsider is that it can be checked.** aledgent's claims are that the
   book is provable, that anchors prove non-revision, and that a recipient can verify a disclosure
   without aledgent (`crates/verify`). An open core that withholds the parts that make those claims
   true would undermine them.
2. **The core cannot be closed anyway.** `agents/` links VOS, and VOS is AGPL-3.0-or-later. Whoever
   runs the core as a network service owes its source to the users who interact with it (AGPL §13).
   Withholding the permissive half (ingress, settle) would not make the product proprietary. It
   would only make it harder to run.
3. **Bloque's advantage is operation, not source.** That means the regulated operator of record,
   the settlement-layer position, and the integrations and support. A competitor who runs aledgent
   still has to build all of those.
4. **Collaboration needs the whole tree.** Outside contributors can only propose changes through
   the SPEC → amendment → PLAN → feature flow (`CONTRIBUTING.md`) if the documents and the code are
   both public.

**Not recommended: dual-licensing the AGPL part.** Selling a commercial licence for `agents/` would
need the consent of every copyright holder of what it links, and VOS's copyright is not Bloque's.
Bloque could dual-license only the code it owns outright, and that code is already permissive, so a
commercial licence would sell nothing. If the owner wants a commercial option later, the
precondition is a CLA (§3) *and* an agreement with VOS's maintainers. Both are recorded as
questions.

**An alternative the owner may prefer:** make the services copyleft too (AGPL-3.0-or-later for
`services/**` and `ops/**`), so that a hosted competitor must publish its changes to ingress and the
adapter. This costs adoption by integrators, who would avoid linking `ingress::remote` or the
sidecar into their own code. The libraries an integrator embeds (`crates/verify`,
`crates/conformance`, the sidecar) should stay permissive either way. It is
question `oss-licence-model`.

## 3. Inbound contributions: DCO, not CLA (ruled 2026-10-05)

- **DCO (Developer Certificate of Origin)** is recommended. Each commit carries `Signed-off-by:`.
  Inbound licence equals outbound licence. It is lightweight and matches a collaborative project,
  and the GitHub DCO app enforces it.
- **A CLA** would be needed only if Bloque wants the right to relicense contributions, for example
  to sell a commercial licence or to move a permissive crate to a different licence later. It deters
  casual contributors and needs a signing service.

This is question `oss-dco-cla`. `CONTRIBUTING.md` is written for DCO and says what changes if a CLA
is chosen.

## 4. Third-party licences and how they fit

Checked on 2026-10-04 with `cargo metadata` and `cargo deny list` over every Cargo workspace in the
tree (root, `agents`, `agents/core`, `agents/shard`, `agents/aledgent-runtime`,
`contracts/settlement`, `spikes`, `services/settle/kreivo-pin`, `services/settle/kreivo-pin-132`,
`e2e/network`), and over every `package-lock.json`.

### 4.1 Direct upstreams

| Upstream | Licence (verified) | Where it is used | Fit |
|---|---|---|---|
| VOS (`vos`, `vos-agent-sdk`, `vos-protocol`, `vos-macros`, `vos-pvm`) | AGPL-3.0-or-later (`LICENSE`, `Cargo.toml`) | `agents/**` only (`tools/ci/check-no-vos.sh` enforces this) | Forces AGPL-3.0-or-later on `agents/**`. Compatible. |
| CipherClerk (`cipher-clerk`) | Apache-2.0 OR MIT (`LICENSE-APACHE`, `LICENSE-MIT`, copyright Daniel Olano) | `crates/ledger-kernel` and through it the core, and `agents/**` (patched) | Compatible with both licences we use. |
| smoldot, smoldot-light | GPL-3.0-or-later WITH Classpath-exception-2.0 (verified in the fork's `LICENSE` and `Cargo.toml`) | `services/settle` (the adapter's light client), from a fork pinned by commit | Compatible: the exception allows linking into an independent module under any licence. Obligations in §5. |
| Kreivo (`kreivo-apis`, `virto-common`) | GPL-3.0-only | `contracts/settlement` (normal dependency), `services/settle/kreivo-pin*` (dev-dependencies only) | See §4.3. |
| frame-contrib (`fc-traits-*`, `frame-contrib-traits`, `pass-authenticators-substrate-keys`) | GPL-3.0-only | `contracts/settlement` through `kreivo-apis`; `kreivo-pin*` (dev-only) | See §4.3. |
| Polkadot SDK (FRAME `sp-*`, `frame-*`) | Apache-2.0 | `kreivo-pin*`, `e2e/network` | Compatible. |
| Polkadot SDK (`polkadot-*-primitives`, `staging-xcm*`) | GPL-3.0-only | `contracts/settlement` through `kreivo-apis` | See §4.3. |
| Polkadot SDK client (`sc-*`) | GPL-3.0-or-later WITH Classpath-exception-2.0 | `e2e/network` (zombienet-sdk), a test harness | Never distributed. No obligation. |
| ink! | Apache-2.0 | `contracts/settlement` | Compatible. |
| polkadot-api, `@polkadot-api/*`, `@polkadot-labs/hdkd*`, `@noble/*` (TypeScript and JavaScript) | MIT | `e2e/papi`, `tools/pass-import` | Compatible. |
| `@polkadot/api`, `@polkadot/api-contract`, `@polkadot/util*` | Apache-2.0 | `contracts/settlement/scripts` | Compatible. |
| `@substrate/connect*` | GPL-3.0-only | pulled in by `@polkadot/api` (`contracts/settlement/scripts`) | Run from source, never bundled or distributed. No obligation. If a bundle is ever published, it is GPL-3.0-only. |
| The docs site's build: Astro, Starlight, `starlight-openapi`, `specsite` (`github:swimmingoncode/specsite`), `sharp` | MIT; `sharp` Apache-2.0 (its prebuilt libvips binaries LGPL-3.0-or-later) | `site/` | Build tools; only the generated pages are published, and they contain none of their code but Starlight's MIT client scripts. Compatible. |
| Fonts: Familjen Grotesk, Geist, Geist Mono (`@fontsource/*`) | OFL-1.1 | `site/`, self-hosted in the published site | The OFL allows embedding and redistribution with the site; the font files keep their licence. |
| `smoldot` (npm) | GPL-3.0-or-later WITH Classpath-exception-2.0 | the same scripts, through polkadot-api's light-client provider | As above. |

### 4.2 The process boundary between AGPL and permissive code

`agents/core-host` (AGPL) serves `ingress::boundary::Core`'s four operations over a local socket.
`ingress::remote` (permissive) is its client. They are separate programs that exchange a narrow,
documented message format, which the FSF treats as separate works. Even so:

- whoever runs aledgent as a service must offer users the **source of the AGPL programs they
  interact with**, including any modification. Users reach the core *through* ingress, and a
  conservative reading of AGPL §13 says that is interaction. Since the whole tree is public, the
  obligation is met by pointing at the exact commit, *and* at the VOS commit it pins (§5);
- no permissive crate may link VOS or `agents/**`. `tools/ci/check-no-vos.sh` enforces this, and it
  must stay a required check.

### 4.3 Flagged: the built settlement contract is GPL-3.0-only

`contracts/settlement` depends on `kreivo-apis` (GPL-3.0-only), which pulls in `frame-contrib-traits`,
`polkadot-core-primitives`, `polkadot-parachain-primitives` and `staging-xcm` (all GPL-3.0-only).
These are statically compiled into the contract's PolkaVM or Wasm blob. So:

- the **source** in `contracts/settlement` can stay Apache-2.0 OR MIT. Permissive code may be
  combined into a GPL work;
- the **compiled contract** (`.contract`, `.polkavm`) is a combined work under **GPL-3.0-only**.
  Anyone who distributes the blob, including uploading its code to a public chain, should publish the
  corresponding source, which this repository does, and should name the licence in the release
  notes;
- an alternative is the one `contracts/settlement/src/kreivo.rs` already takes for part of the
  binding: write the few chain-extension types locally instead of depending on `kreivo-apis`. That
  would make the blob permissive. It is a small change, and it is not made here.

### 4.4 Everything else

Every other dependency in every workspace is under a permissive licence: MIT, Apache-2.0, BSD-2/3,
ISC, Zlib, Unicode-3.0, CC0-1.0, 0BSD, BSL-1.0, BlueOak-1.0.0, Python-2.0 (`argparse`, dev only), or
CC-BY-3.0 (`spdx-exceptions`, data). Two crates are MPL-2.0: `webpki-roots` and `attohttpc` in
`e2e/network`. MPL-2.0 is file-level copyleft. It applies only if
those files are modified, and they are not.

## 5. Obligations that come with the release

1. **Keep the forks public and reachable.** The tree pins a modified smoldot and a modified VOS.
   The GPL and the AGPL require that whoever distributes, or serves, a build can get the
   corresponding source. The smoldot fork lives under Bloque's organisation
   (`github.com/bloque-app/smoldot`). The VOS fork is mirrored there (`github.com/bloque-app/vos`), and
   the core still pins it at `codeberg.org/pandres95/VOS`, whose URL is part of the core package's
   bytes, until code upgrades (`F-037`) let it move. Keep every pinned commit, at both places, forever.
2. **Keep the Classpath exception in the smoldot fork.** It is in the fork's `LICENSE` today. The
   exception lets a modified copy keep it, but does not oblige it, so the fork must not drop it.
3. **Release binaries with a source offer.** Container images of `agents/core-host` (AGPL) and of
   the services that link smoldot (GPL with the exception) should carry a label or a file that
   points at the source commit. The adapter image should also name the smoldot fork commit.
4. **Do not let `agents/**` grow permissive dependents.** This is the check above.

## 6. The owner's rulings (2026-10-05 and 2026-10-13)

The owner answered the release questions of 2026-10-04 on 2026-10-05, and signed off on 2026-10-13:

- **Copyright holder: Pagos Cube LLC**, Bloque's operating company. Every header, `LICENSE-MIT`,
  `REUSE.toml` and `deny.toml` say `Copyright (c) 2026 Pagos Cube LLC`; "Bloque" stays only as the
  brand. `tools/oss/spdx.py`'s `HOLDER` writes it into new files.
- **Licence model: fully open, as §1 and §2 describe.** No dual licence, no open core.
- **Inbound: DCO.** `CONTRIBUTING.md` describes it, the pull-request template asks for it, and
  `.github/workflows/dco.yml` checks every commit of a pull request once the repository is public.
- **Trademark: keep "aledgent".** A clearance search and a filing are waived for now (2026-10-13): the
  open-source family and the parent brand are expected to change in the coming months. `GOVERNANCE.md`
  says the licences grant no use of the name, and the marks in `docs/brand/` are not licensed with the
  code (`LicenseRef-Trademark-Marks`).

**IP: covered (2026-10-13).** About half the history's commits carry the author's personal address,
not the company one. The author's consultancy contract assigns the IP of Bloque's products to Pagos
Cube LLC, so that work is the company's, and the company's release authorization records it. The same
holds for the two forks the build pins (§5, item 1).

## 7. How it is enforced

- `tools/oss/spdx.py --check`: every tracked file that can carry a comment has an
  `SPDX-License-Identifier` and a copyright line, and the licence is the one its path requires
  (`agents/**` is AGPL, prose is CC-BY-4.0, everything else Apache-2.0 OR MIT). `--fix` adds missing
  headers.
- `reuse lint`: every file, including those that cannot carry a comment, has licence information
  (`REUSE.toml`), and every licence used has its text in `LICENSES/`.
- `tools/ci/check-no-vos.sh` (existing): no crate outside `agents/` and `spikes/` names a VOS crate,
  and every crate that does declares AGPL-3.0-or-later.
- `deny.toml` with `cargo deny check licenses`: every dependency's licence is on the allow-list
  above. A new copyleft dependency fails the build until someone decides about it here.

All three run in the `licensing` job of `.github/workflows/ci.yml`.
