<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: turning on approvals, and moving `payment-rails` first

`REQ-AU-23` (amendment 0037) · `T-040-07` · `T-040-14` · `NFR-17`

**Owner:** the `payment-rails` side owns moving its calls; the operator owns the switch. The step is done
when `payment-rails` says so and the counts below agree.
**Rule that does not bend:** enforce is not turned on in a deployment while report only still counts calls
made for a person's session. Such a call is refused under enforce.

1. **Report only.** Set ingress's `approvals` setting to `report_only` (and `environment`, which it needs).
   Nothing is refused. Ingress logs the kind of each command that would be refused and counts commands by
   how the caller proved who it is: session, signature, issued secret, configured token.
2. **`payment-rails` moves.** For each person's session call it makes, it asks for the digest first, shows the
   person the command, has the device approve it, then sends the command with the approval (the page
   "Approving a command", under Integrate). Calls it makes with its issued secret or configured token do not move.
3. **Watch.** Session counts fall to zero for `payment-rails`, and the refusals ingress would have made fall
   with them. Anything that remains is a caller that was not expected: find it before going on.
4. **Enforce.** Only when the session count has been zero over the period the owner chose. Set `approvals` to
   `enforce`. Keep watching `ERR-CommandApprovalRequired` and `ERR-CredentialInvalid` for a day.
5. **Back.** Set `approvals` back to `report_only`. It takes effect on restart of ingress. Nothing is lost:
   a command taken in under enforce stays taken in.
