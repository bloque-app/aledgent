<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# What a caller does about each refusal

The error catalogue (`/errors/`) is generated from three sources at every build of the site:
the condition of each `ERR-*` in `SPEC.md` §10, its HTTP status from ingress's OpenAPI document
(`x-http-status`), and the section below with the same identifier, which says what the caller does. The
build fails when the three disagree: an `ERR-*` in §10 without a section here, a section for an `ERR-*`
§10 does not name, or an `ERR-*` the OpenAPI document does not carry. Each section starts with a line
`Seen by:` and a line `Retry:`, then the action, in Markdown.

## ERR-NotAuthorised

Seen by: every caller.
Retry: no, not unchanged.

Nothing you hold covers this command. Work out which authority the call relies on and check it:

- **Your own account:** you own it, and your credential's `scope` (if it has one) includes it.
- **Someone else's account:** a delegated grant to you, over that account (`subject`), naming this
  operation kind in `ops`, with the asset and counterparty inside its limits, not yet past `expires`.
- **Issuing or redeeming:** an issuance grant over the asset.
- **An operator command:** a live operator grant of that kind, held by the operator identity. The operator
  holds nothing by default ([Least privilege](../../operate/least-privilege/)).

A grant that lapsed (its `expires` passed) gives this condition, not `ERR-AuthorityRevoked`. So does an
account you cannot reach: aledgent does not tell you whether it exists. Ask whoever grants the authority
for a grant that covers the call; see [Grants](../../integrate/grants/).

## ERR-AuthorityRevoked

Seen by: a holder of delegated, issuance or operator authority.
Retry: no.

The grant you relied on existed and was revoked. A revocation takes effect at once and is never undone:
ask the grantor for a new grant. If you hold several grants over the same account, the one that was
used is the one that was revoked. Check `GET /v1/identities/{identity}/grants` for what you still hold.

## ERR-CredentialInvalid

Seen by: every caller.
Retry: only with a different credential.

The credential you presented did not authenticate. One condition covers every cause on purpose
(`REQ-AU-8`): a malformed or unknown API key, a removed credential, an expired session, a passkey
assertion or approval that did not verify, a `key` proof that did not verify. Check, in order:

- the header is `Authorization: Bearer <token>`, with nothing else around the token;
- an API key has the form `aledgent_ak1.<credential id body>.<secret>`, with the secret in base64url
  without padding; see [API keys from zero](../../integrate/api-keys/);
- the credential has not been removed;
- a session from a passkey sign-in has not expired: sign in again.

A lost API-key secret cannot be recovered. Issue a new key and remove the old credential.

## ERR-RecoveryNotElapsed

Seen by: a holder completing a recovery or claiming a succession.
Retry: yes, after the delay.

The recovery or succession has a delay, and it has not elapsed. The reply that opened it carried
`effective_at`: complete it after that time. Until then the holder can cancel it with an existing
credential of the set, through `cancel_recovery` (`POST /v1/recoveries/{pending}/cancel`, `REQ-AU-16`).

## ERR-UnboundedGrant

Seen by: whoever grants authority.
Retry: after adding a bound.

Every grant must carry at least one bound (`REQ-AU-2`): `per_operation`, `per_window`
(`{"length": <ms>, "limit": "<minor units>"}`), an `assets` list or a `counterparties` list. An issuance
grant needs a `per_window` bound and an `assets` list that names its asset. Add the bound and send it
again under a new client key. See [Grants](../../integrate/grants/).

## ERR-BoundExceeded

Seen by: a holder of delegated or issuance authority.
Retry: not unchanged.

The command would move more than the grant allows: over `per_operation`, or over what is left of the
current `per_window`. Send a smaller amount, wait for the window to move on, or ask the grantor for a
wider grant. A refused command took nothing in and spent nothing of the window.

## ERR-UnknownAccount

Seen by: every caller.
Retry: no.

No such account, or none you can reach. aledgent gives the same answer for both, so that a caller cannot
probe for accounts. Check that you sent the account identifier (`did:aledgent:acct:…`), not an identity
or an operation reference, and that your authority reaches it.

## ERR-AccountFrozen

Seen by: every caller debiting the account.
Retry: no, until it is unfrozen.

The operator froze the account. A frozen account refuses every operation that debits it, and still
accepts credits (`REQ-LA-7`, `INV-9`): value can be paid into it, none can leave it. Its balances do not
change and stay `available`. Contact the deployment's operator; there is nothing to fix in the request.

## ERR-AccountClosed

Seen by: every caller.
Retry: no.

The account is closed, and a closed account never reopens. Use another account of the same owner, or
open one.

## ERR-AccountNotEmpty

Seen by: an account's owner or a holder of `close_account`.
Retry: after emptying it.

An account closes only at a zero balance in every asset. Move the balance out (and release any open
reservation against it), then close it again.

## ERR-IdentityRestricted

Seen by: every caller.
Retry: no.

The identity that owns the account is not allowed to transact. This is the deployment's decision, not a
fault in the request. Contact the deployment's operator.

## ERR-UnknownAsset

Seen by: every caller.
Retry: after fixing the asset id.

No asset with this id is in the registry. The causes:

- **It is not registered** (or was removed). Read the registry with `GET /v1/assets`.
- **It cannot be an asset id**: not 2 to 12 ASCII letters and digits. On a command this is
  `ERR-UnknownAsset`; in a read's query parameter it is `ERR-MalformedCommand`.

Case is never the cause: aledgent normalises every asset id it is sent to uppercase, on every route
(`REQ-AS-3`), so `crd` and `CRD` name the same asset. Replies always carry the uppercase form; compare
against that form in your own records.

## ERR-AssetDeprecated

Seen by: every caller.
Retry: no.

The asset is registered but takes no new operation. Existing balances stay readable. Ask the deployment's
operator which asset replaces it.

## ERR-PrecisionExceeded

Seen by: every caller.
Retry: after fixing the amount.

The amount has more precision than the asset. Every asset has one precision (`GET /v1/assets`, field
`precision`), and amounts are sent as decimal-integer strings of minor units at that precision. Convert
before you send; never send a decimal point.

## ERR-ZeroAmount

Seen by: every caller.
Retry: no.

A movement must move something. Do not send a zero amount; if there is nothing to move, send nothing.

## ERR-InsufficientAvailable

Seen by: every caller.
Retry: not unchanged.

The account's available balance does not cover the command. Available is the balance less what open
reservations hold. Read it with `GET /v1/balances`. For a transfer or a reservation, this usually
arrives as the decision after a `202 Received`: the operation ends `Rejected` with this condition, read
by notification or with `GET /v1/operations/{reference}`.

## ERR-AmountMismatch

Seen by: whoever captures a reservation.
Retry: not unchanged.

A capture cleared more than the reservation holds (`INV-14`). A capture may clear the reserved amount or
less, and the difference returns to the payer in the same posting; never more. Capture at most the
reserved amount, and charge any excess separately. For charges whose cost is known only afterwards, see
[Metered usage](../../integrate/metered-usage/).

## ERR-ReleaseUnsafe

Seen by: whoever releases a reservation, and the operator.
Retry: no, until the external leg's outcome is known.

The reservation carries an external leg (a withdrawal) that could still execute on the settlement layer,
so releasing it could pay twice. Wait for the leg's outcome: the operation moves on when the adapter
reports it, or reaches `Needs-Review` past its deadline, where the operator resolves it.

## ERR-OperationTerminal

Seen by: every caller.
Retry: no.

The operation has already reached its final state. Read it with `GET /v1/operations/{reference}`: the
outcome you wanted may already be there.

## ERR-InvalidTransition

Seen by: every caller.
Retry: after the operation moves on, if ever.

The operation is not in a state where this command applies. The usual case: capturing or releasing a
reservation that is still `Received`. Read the operation, and act once it is `Reserved`.

## ERR-UnknownOperation

Seen by: every caller.
Retry: no.

No operation with this reference, or none you can see. Use the `operation_reference` from the reply
(`did:aledgent:op:…`), not your client key and not your own `reference` data.

## ERR-DeadlineRequired

Seen by: every caller.
Retry: after adding a deadline.

The operation needs a deadline and none was given or configured. Send `deadline` (milliseconds since the
Unix epoch) with the command.

## ERR-ClientKeyReused

Seen by: every caller.
Retry: with a new client key, if it is a new request.

The client key was already used for a different request. A retry must be the same request: same body,
same pass-through `reference`, same principal. If this is a new request, give it a new key. Two
principals of the same integrating party share one key space, so keep their keys apart (prefix them, for
example). Never reuse a key for "the next" charge.

## ERR-BatchTooLarge

Seen by: every caller, and the operator.
Retry: after splitting.

The submission is larger than the deployment's declared maximum. Two cases:

- **A transfer with more legs than the published maximum.** Split it into several transfers, each with
  its own client key. Each is atomic on its own; aledgent makes no promise across them.
- **A page of an asset's sweep** (the operator retiring an asset, `REQ-AS-10`) larger than the maximum.
  Send smaller pages.

## ERR-UnsupportedLeg

Seen by: every caller, and the operator.
Retry: no.

The settlement adapter does not support this external operation or asset. A book-only asset has no
external leg at all: its value enters by issuance and leaves by redemption. If the asset should reach the
settlement layer, the operator binds it first.

## ERR-SettlementLayerUnavailable

Seen by: a caller withdrawing, and the operator.
Retry: yes, later.

The settlement adapter cannot be reached, or has declared that it does not serve. Withdrawals are refused
while it is so; nothing was taken in. Internal transfers, reservations and reads keep working. Retry the
withdrawal later.

## ERR-DecisionUnavailable

Seen by: every caller.
Retry: yes, with the same client key, after `Retry-After`.

The command could not be decided in time, usually because no core is connected just now. It is **not a
failure**: the command is kept and will be decided. Retry it with the **same client key** after the
`Retry-After` header's seconds; the retry returns the decision once it exists. Never change the key on
retry, or the command may happen twice. See [While the core is away](../../integrate/core-away/).

## ERR-ObservationDuplicate

Seen by: the settlement adapter.
Retry: no.

An observation at this coordinate was already applied. Treat it as done and move on.

## ERR-ObservationUnassociated

Seen by: the settlement adapter.
Retry: no.

The observation is valid but matches no operation the book holds. Record it for the operator, who
resolves it through reconciliation.

## ERR-EvidenceInsufficient

Seen by: the settlement adapter and the operator.
Retry: with better evidence.

The evidence does not establish the outcome claimed. Submit the settlement layer's proof for the
outcome. If no proof exists, the operation is resolved by the operator's two-principal release instead.

## ERR-JustificationRequired

Seen by: the operator.
Retry: after adding a justification.

Every operator command carries a `justification`. Say why, and send it again.

## ERR-ApprovalRequired

Seen by: the operator.
Retry: after a second principal approves.

A reversal, or a release without proof, needs a second, distinct principal to approve it. Have that
principal approve it (`POST /v1/ops/reversals/{reference}/approve` for a reversal). The proposer cannot
approve its own.

## ERR-CommandApprovalRequired

Seen by: a client acting under a person's session.
Retry: with an approval.

The command needs the person's approval of this one command, and had none. Ask for an approval and send
it; see [Approving a command](../../integrate/approving-a-command/).

## ERR-DisclosureUnsupported

Seen by: a caller asking for, or verifying, a disclosure.
Retry: no.

aledgent cannot make the assertion asked for. Ask for an assertion it supports. When verifying, check that
the disclosure was sent, as hex.

## ERR-DisclosureFalse

Seen by: a caller asking for a disclosure.
Retry: no.

aledgent can tell the assertion is false, and will not sign it. Nothing is wrong with the request.

## ERR-NotDisclosable

Seen by: a caller asking for a disclosure.
Retry: after the subject authorises it.

The subject has not authorised this disclosure, and no legal obligation applies. Ask the account's owner
for a grant that names `disclose`.

## ERR-AnchorNotFinal

Seen by: a caller citing an anchor.
Retry: yes, later.

The anchor cited has not reached finality on the settlement layer. Wait for it, or cite an earlier anchor
that is final.

## ERR-QueryUnbounded

Seen by: every caller reading.
Retry: after adding bounds.

The read is missing a bound aledgent needs, or its bounds conflict: no subject or two, a page size outside
1 to 1000, half a time range, a cursor from other filters. See
[Reading the book](../../start/reading-the-book/).

## ERR-MalformedCommand

Seen by: every caller.
Retry: after fixing the request.

The request does not have a shape aledgent takes, and nothing was taken in. The refusal names no field, on
purpose. Check the body against the [API reference](../../api/). Common causes: an unknown field
(bodies reject unknown fields), an amount sent as a JSON number instead of a string, a malformed
`did:aledgent:` identifier, a client key over 256 characters, `reference` data over 1 KiB.

## ERR-InitialStateNotSealed

Seen by: every caller of a new deployment.
Retry: yes, once the deployment is sealed.

The book's initial state is not in place and checked yet, and the book takes no command until it is
(`REQ-RC-6`). This is the operator's to finish; retry once the deployment is sealed.

## ERR-IdentifierArchived

Seen by: every caller of a migrated deployment.
Retry: no.

The identifier is one a previous system issued, which the migration deliberately left out. It names no
account and never will. Stop using it, and map the customer to the account they hold now.

## ERR-Internal

Seen by: every caller.
Retry: yes, with the same client key, later.

A fault inside aledgent. No detail is returned, and the operator's incident is already raised. A retry
with the same client key is safe: it never makes a command happen twice. If it persists, tell the
operator the time and the client key.
