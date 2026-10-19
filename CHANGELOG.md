<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Changelog

Releases are cut by release-please from the conventional-commit titles of the pull requests merged
into `main` (`release-please-config.json`). Each entry below is written by its release pull request.

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
