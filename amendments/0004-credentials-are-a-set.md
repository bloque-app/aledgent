<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0004 — there is no unit of owned custody

**Status:** applied to `SPEC.md` v0.3.2
**Target:** `SPEC.md` v0.3.1
**Date:** 2026-09-21
**Rationale in one line:** `OQ-13` asked which single thing a credential attaches to — identity, account
or device — and the authority model never needed one.

## The problem

The question inherited a premise from a system where one on-chain address meant one key. In this
specification custody mode is a property of the account, several principals may hold authority over one
account, and a credential is merely what proves an authority. Nothing forces one credential per identity
or one credential per account.

## The amendment

`REQ-AU-20` (new): an identity holds a **set** of credentials, each belonging to one identity; owner
authority over any owned account may be exercised by any credential in the set, optionally scoped to a
subset of accounts; membership changes require an existing credential or a completed recovery; recovery
reissues into the set; the holder-held recovery credential is a marked member; where a credential is
stored is not a ledger fact. `AC-A5.4` added; commands `register_credential` / `remove_credential` added
to §8.1; `OQ-13` closed.

## What this costs, stated plainly

A holder with several credentials has several ways to be compromised; scoping a credential to a subset
of accounts is the mitigation the spec offers, and it is the holder's to use.

## Closed

`OQ-13`. **No open question blocks V0 any longer.**

## Carried to the plan

`F-003` models credentials as a per-identity set with optional per-account scope; `F-012` is unblocked
(`DEP-6` resolved).
