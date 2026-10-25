<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Changelog

Releases are cut by release-please from the conventional-commit titles of the pull requests merged
into `main` (`release-please-config.json`). Each entry below is written by its release pull request.

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
