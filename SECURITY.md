<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Security policy

aledgent is a ledger of record. We take reports about it seriously and would rather hear about a
problem privately, early, than read about it.

## Reporting a vulnerability

**Do not open a public issue, discussion or pull request for a security problem.**

Report it privately through GitHub's **private vulnerability reporting**: on this repository's
*Security* tab, choose *Report a vulnerability*. That opens an advisory that only you and the
maintainers can see.

Please include:

- what is affected (component, commit or release) and how to reproduce it;
- what an attacker can do with it: move value, read what they should not, deny service, forge an
  anchor or a disclosure;
- any proof of concept, and whether you have tested against a live deployment (please do not; see
  below).

## What to expect

- We acknowledge a report within **three working days**.
- We give a first assessment, with severity and next steps, within **ten working days**.
- We agree on a disclosure date with you. Our default is to publish an advisory when a fix is
  released, and within 90 days of the report at the latest. We credit you in the advisory unless you
  ask us not to.

## Scope

In scope: everything in this repository. That includes the core (`crates/`, `agents/`), ingress and
the settlement adapter (`services/`), the settlement contract (`contracts/`),
the verifier, and the deployment templates.

Problems in upstream projects (VOS, CipherClerk, Kreivo, smoldot, the Polkadot SDK) should go to
those projects. If one affects aledgent, tell us as well and we will coordinate.

## Safe harbour

We will not pursue legal action against anyone who, in good faith, researches and reports a
vulnerability under this policy, provided they do not access or modify other people's data or funds,
do not degrade a live service, and give us reasonable time to fix the problem before disclosing it.
Test against your own deployment. The end-to-end suite (`e2e/README.md`) starts a local network for
exactly this.

## Known limitations

`SPEC.md` and the conformance suite's assurance declaration state what aledgent does *not* yet
enforce. For example, the kernel's privacy layers beyond amount and access hiding are unaudited and
disabled (`DEF-4`), and disclosures are signed by aledgent rather than checkable by a construction
(`DEF-3`). Reports that restate a declared limitation are welcome as discussions, not as
vulnerabilities.
