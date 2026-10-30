<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Changelog

Releases are cut by release-please from the conventional-commit titles of the pull requests merged
into `main` (`release-please-config.json`). Each entry below is written by its release pull request.

## [0.4.0](https://github.com/bloque-app/aledgent/compare/v0.3.1...v0.4.0) (2026-10-29)


### Features

* **contract:** T-043-02 chained settlement entries, one release with a full reset ([#78](https://github.com/bloque-app/aledgent/issues/78)) ([d76deef](https://github.com/bloque-app/aledgent/commit/d76deeff23da3e38ae9cef464d7ce6f0c2eb44c1))
* **core-archive:** read short-lived S3 credentials from a file an agent keeps fresh ([#70](https://github.com/bloque-app/aledgent/issues/70)) ([4fef3ad](https://github.com/bloque-app/aledgent/commit/4fef3ad9080e4beadbfa91ec9cae9ec8d879cb15))
* **core-archive:** the archive sealed by aledgent, Vault Transit and an offline recovery key (F-044) ([#82](https://github.com/bloque-app/aledgent/issues/82)) ([622dd6a](https://github.com/bloque-app/aledgent/commit/622dd6a097289d3286ac2bcceb5ade0c6c04150a))
* **core-host:** T-037-09 restore across upgrades, staging every version the archive names ([#86](https://github.com/bloque-app/aledgent/issues/86)) ([25b0f96](https://github.com/bloque-app/aledgent/commit/25b0f96827c112e756f9fda2c1e1e902e5f1cc0f))
* **core:** rules version 1, the refusal line, amendment 0041 and the grant-choice fix (T-041-06) ([#102](https://github.com/bloque-app/aledgent/issues/102)) ([eee7c4d](https://github.com/bloque-app/aledgent/commit/eee7c4d8aae229fe08bce9fbcfff5571749ea848))
* **core:** T-037-09 restore and replay per range, under the code map read from the inputs ([#85](https://github.com/bloque-app/aledgent/issues/85)) ([7e23ac3](https://github.com/bloque-app/aledgent/commit/7e23ac32546f0f29e20ea54242a624221f6ccecb))
* **core:** T-043-06 the re-scan refused from the chained generation ([#91](https://github.com/bloque-app/aledgent/issues/91)) ([77831be](https://github.com/bloque-app/aledgent/commit/77831be0de4462027312a7ff736c72af8e4c2cd2))
* **core:** T-043-07 settlement incidents and gaps to the audit trail ([#92](https://github.com/bloque-app/aledgent/issues/92)) ([1788745](https://github.com/bloque-app/aledgent/commit/1788745bbe82d132ea12cf6045fccd8c3ecd0335))
* **core:** T-043-08 running books move their incidents and gaps to the audit trail in place ([#93](https://github.com/bloque-app/aledgent/issues/93)) ([9c12b11](https://github.com/bloque-app/aledgent/commit/9c12b11ab5b0ccef26f52cd7439869a94ed11f84))
* **core:** T-043-14 the core follows a new settlement sequence (amendment 0041) ([#96](https://github.com/bloque-app/aledgent/issues/96)) ([65a0871](https://github.com/bloque-app/aledgent/commit/65a08715e6867fd558d7979cf1f760c5e572dc1e))
* **core:** T-043-17 no withdrawal from a stale settlement head (amendment 0041) ([#99](https://github.com/bloque-app/aledgent/issues/99)) ([1ad067b](https://github.com/bloque-app/aledgent/commit/1ad067ba3e5bb6fd07d85ec211ede4536b8f4444))
* **ingress:** T-043-16 GET /v1/ops/settlement/head (amendment 0041) ([#98](https://github.com/bloque-app/aledgent/issues/98)) ([1bdb26e](https://github.com/bloque-app/aledgent/commit/1bdb26e1913055da7111bc4d04cdabdaf5fa6308))
* promote develop to main (rules version 1, amendment 0041, chained settlement, signature engines) ([c286c7c](https://github.com/bloque-app/aledgent/commit/c286c7cd53d142e51a74e24e24bd60d2940848ef))
* **settle:** F-045 lanes and the anchor key as native ed25519 accounts, keys in Vault Transit ([#81](https://github.com/bloque-app/aledgent/issues/81)) ([f91ca4d](https://github.com/bloque-app/aledgent/commit/f91ca4d7c95d45153cf208d056972bd3d101b62e))
* **settle:** T-027-12 sign on Kreivo 0.18.1 (spec 136, transaction_version 14) ([#89](https://github.com/bloque-app/aledgent/issues/89)) ([54274b7](https://github.com/bloque-app/aledgent/commit/54274b7a3a2d2006ae147c4cfbb7bf85f2759ff5))
* **settle:** T-043-04/05 verify the chain of entries at sync; re-scan removed ([#79](https://github.com/bloque-app/aledgent/issues/79)) ([fb93f28](https://github.com/bloque-app/aledgent/commit/fb93f2818d433c05811fb9cb874906804b39b6f0))
* **settle:** T-043-15 the adapter reports a new settlement sequence (amendment 0041) ([#97](https://github.com/bloque-app/aledgent/issues/97)) ([6a17392](https://github.com/bloque-app/aledgent/commit/6a1739293f1eae3dcb85a62884cccef17ae8b5ab))
* **signer:** signature engines, Vault Transit first (F-042) ([#71](https://github.com/bloque-app/aledgent/issues/71)) ([56c5273](https://github.com/bloque-app/aledgent/commit/56c52734bc1f20fc12af8c1fa72d27aa13afb007))
* **verify:** T-043-09 chained-design readers, alerts and runbooks ([#84](https://github.com/bloque-app/aledgent/issues/84)) ([df6b1e5](https://github.com/bloque-app/aledgent/commit/df6b1e5d30103e4cdfcddecec1704db7f0070f95))


### Bug Fixes

* **core:** T-003-11 choose among the grants that list the operation's asset, from generation 1 ([#101](https://github.com/bloque-app/aledgent/issues/101)) ([fdde2fd](https://github.com/bloque-app/aledgent/commit/fdde2fd427ab4540688b6bdc0eb89788aad962cd))
* **ingress:** T-043-19 do not send the settlement head read to a core that cannot decode it ([#103](https://github.com/bloque-app/aledgent/issues/103)) ([f6168e4](https://github.com/bloque-app/aledgent/commit/f6168e47e2fa2c718554a38b379978e1b00f1bc1))
* **ingress:** T-043-19 the settlement head read is not sent to a core that cannot decode it ([f6168e4](https://github.com/bloque-app/aledgent/commit/f6168e47e2fa2c718554a38b379978e1b00f1bc1))
* **settlement:** record a re-scan's difference once, however often it is found (T-032-25) ([#73](https://github.com/bloque-app/aledgent/issues/73)) ([c79d12d](https://github.com/bloque-app/aledgent/commit/c79d12db82b8ff05e8f975ed7bd8b9ad445b573d))

## [0.3.1](https://github.com/bloque-app/aledgent/compare/v0.3.0...v0.3.1) (2026-10-25)


### Bug Fixes

* **ingress:** write a movements limit the way the core reads it ([#65](https://github.com/bloque-app/aledgent/issues/65)) ([f448982](https://github.com/bloque-app/aledgent/commit/f44898200f230baeb033efd21888bb328d5c9324))
* promote develop to main (movements read over the core stream) ([913c9ec](https://github.com/bloque-app/aledgent/commit/913c9ecb61d7f683f41646cbb6b8c7d866f66997))

## [0.3.0](https://github.com/bloque-app/aledgent/compare/v0.2.0...v0.3.0) (2026-10-23)


### Features

* **core:** T-037-05 the upgrade-authority modes and their inputs ([#45](https://github.com/bloque-app/aledgent/issues/45)) ([9a3160e](https://github.com/bloque-app/aledgent/commit/9a3160e661f06f9cbcf7dbb17eaa93d30322c46c))
* **core:** T-037-06 the fetcher, the fs code source and serve --code-config ([#54](https://github.com/bloque-app/aledgent/issues/54)) ([c747dda](https://github.com/bloque-app/aledgent/commit/c747dda228ad838befd4dcbc2eb96881fb7c20f1))
* **core:** T-037-14 switch(H, n): the driver switches the code before it sends the input that needs it ([#46](https://github.com/bloque-app/aledgent/issues/46)) ([15f50aa](https://github.com/bloque-app/aledgent/commit/15f50aa6ecdc3873992622f965d00fa93c376dbb))
* **core:** T-037-15 the fence, always on, and the way out ([#44](https://github.com/bloque-app/aledgent/issues/44)) ([4f9d053](https://github.com/bloque-app/aledgent/commit/4f9d053374dca61ad130a00eec1d225f8e4550da))
* **core:** T-037-19 the core host installs a new generation with the patched VOS (ruling w5) ([#49](https://github.com/bloque-app/aledgent/issues/49)) ([d958b63](https://github.com/bloque-app/aledgent/commit/d958b6319e3099cd70f1d73c341b7f113a0455ea))
* **core:** T-037-21 the core restarts on the code versions the book names ([#55](https://github.com/bloque-app/aledgent/issues/55)) ([544c90c](https://github.com/bloque-app/aledgent/commit/544c90ce15ea9f946d99b75673a3a78923875123))
* **core:** T-037-22 the door refuses a version the switch cannot install, T-037-23 see which program the core runs ([#57](https://github.com/bloque-app/aledgent/issues/57)) ([6552103](https://github.com/bloque-app/aledgent/commit/65521031846e21f70bb5cc569eec967ddfc1262b))
* **ingress:** T-037-18 the fence answers not-run and the feeder gives sequences back (ruling w7) ([#51](https://github.com/bloque-app/aledgent/issues/51)) ([c3461c0](https://github.com/bloque-app/aledgent/commit/c3461c063ad3d4b5626f7d629191e0c4146020d7))
* **ingress:** T-040-02..13 per-transaction passkey approval at the front door (amendment 0037) ([#59](https://github.com/bloque-app/aledgent/issues/59)) ([85432f5](https://github.com/bloque-app/aledgent/commit/85432f54ea8b47975223dbd45efc92816e3f809b))
* **ledger:** T-037-05 a rescheduled upgrade cannot move further than the authority-change lead (ruling w9) ([#53](https://github.com/bloque-app/aledgent/issues/53)) ([ff79ef1](https://github.com/bloque-app/aledgent/commit/ff79ef11b26fd4fbc6e8c39e5efbc81a01f266d2))
* **ledger:** T-037-05 a rescheduled upgrade moves strictly later than the scheduled input (ruling w6) ([#48](https://github.com/bloque-app/aledgent/issues/48)) ([556dc3f](https://github.com/bloque-app/aledgent/commit/556dc3facafdd0d06230abc36c529732cead1f34))
* **ops:** per-book anchor reads so the drill and agent check survive a settlement reset (T-026-12) ([#62](https://github.com/bloque-app/aledgent/issues/62)) ([ea23bf2](https://github.com/bloque-app/aledgent/commit/ea23bf2ef4f957b1151b3cbd7eba1a34fa59187e))
* **ops:** T-037-04 the Upgrade input through CTR-OPS ([#47](https://github.com/bloque-app/aledgent/issues/47)) ([d645291](https://github.com/bloque-app/aledgent/commit/d64529101b3a2381c60575009ceaa33e251ba8b1))
* promote develop to main (F-040 approvals, T-037 code machinery, settlement contract v2, per-book anchor reads) ([1d2ef4d](https://github.com/bloque-app/aledgent/commit/1d2ef4d3b23c7a1b1b12f3cf5fa449f5413959fe))
* **settlement:** governance-gated anchor head reset, no migration needed (contract v2, T-026-10) ([#60](https://github.com/bloque-app/aledgent/issues/60)) ([210ca4b](https://github.com/bloque-app/aledgent/commit/210ca4bb13713780586882c2a6512ab6b39ed256))
* **verify:** read a re-founded book's anchors by sequence (T-026-11) ([#61](https://github.com/bloque-app/aledgent/issues/61)) ([160dce7](https://github.com/bloque-app/aledgent/commit/160dce7c88e3bd3ce9a278da1901fac49da54b78))

## [0.2.0](https://github.com/bloque-app/aledgent/compare/v0.1.2...v0.2.0) (2026-10-19)


### Features

* **core-host:** measure --mix open, the profiling mix that opens identities and accounts ([#15](https://github.com/bloque-app/aledgent/issues/15)) ([5e3b76d](https://github.com/bloque-app/aledgent/commit/5e3b76d8324204be78767172ed4a939922b68165))
* **images:** a core image with the current host on a deployment's genesis bytes ([#28](https://github.com/bloque-app/aledgent/issues/28)) ([3751d54](https://github.com/bloque-app/aledgent/commit/3751d5498dfa46b0541233d1d0f66a36a55b65c0))
* **ingress/core-host:** F-038, ingress as its own fleet with cores connecting out ([#14](https://github.com/bloque-app/aledgent/issues/14)) ([45a2ff1](https://github.com/bloque-app/aledgent/commit/45a2ff13b602ca13506f1285068f5642e041be2b))
* **ingress:** every read refuses a shape fault as ERR-MalformedCommand (400), amendment 0032 proposed ([#34](https://github.com/bloque-app/aledgent/issues/34)) ([94b2912](https://github.com/bloque-app/aledgent/commit/94b2912e18303518a7909abb0e1569c25666b517))
* **settle:** choose the transaction format by the runtime's surface (T-027-12) ([#13](https://github.com/bloque-app/aledgent/issues/13)) ([7d25ad1](https://github.com/bloque-app/aledgent/commit/7d25ad1c8fef6bf7a9d8df38cbf71fc397a38a8f))
* **settle:** T-027-12 follow-ups from its review ([#17](https://github.com/bloque-app/aledgent/issues/17)) ([a806f39](https://github.com/bloque-app/aledgent/commit/a806f39580a58490cde867798a0d4a9f2b88e3dc))


### Bug Fixes

* **ops:** NFR-4 does not hold the settlement adapter's replies to the book as operations ([#32](https://github.com/bloque-app/aledgent/issues/32)) ([25d2631](https://github.com/bloque-app/aledgent/commit/25d26311f7d48b71866d92e72fcbb960d624a1c5))
* **ops:** restore core reads the archive in parallel and runs on one image ([#29](https://github.com/bloque-app/aledgent/issues/29)) ([416755b](https://github.com/bloque-app/aledgent/commit/416755bb2e98ca6e1a4e6da89c067df5b07620d8))
* **release:** sign off release commits as the aledgent-release App ([5c27bb0](https://github.com/bloque-app/aledgent/commit/5c27bb0e4ab9ff009f7acd40659b533763e83581))
* **release:** sign off release commits as the aledgent-release App ([#42](https://github.com/bloque-app/aledgent/issues/42)) ([77963c4](https://github.com/bloque-app/aledgent/commit/77963c47a9de9a1ea6b9f0f1673ad9ea71292de6))

## [0.1.2](https://github.com/bloque-app/aledgent/compare/v0.1.1...v0.1.2) (2026-10-14)


### Bug Fixes

* **settle:** sign for Kreivo spec 134 ([#7](https://github.com/bloque-app/aledgent/issues/7)) ([ce6a6f6](https://github.com/bloque-app/aledgent/commit/ce6a6f65a29027e1cd6f08c7ae71b8522f6e5084))

## [0.1.1](https://github.com/bloque-app/aledgent/compare/v0.1.0...v0.1.1) (2026-10-14)


### Miscellaneous Chores

* release 0.1.1 ([#5](https://github.com/bloque-app/aledgent/issues/5)) ([1853316](https://github.com/bloque-app/aledgent/commit/185331600e3f5bac40d171cb127fca6b26be781f))

## 0.1.0 (2026-10-13)

The initial public release of aledgent. It was not tagged: the first release cut from this
repository compares against it.
