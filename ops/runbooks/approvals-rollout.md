<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: turning on approvals, and moving `payment-rails` first

`REQ-AU-23` (amendment 0037) · `T-040-07` · `T-040-14` · `NFR-17`

**Owner:** the `payment-rails` side owns moving its calls; the operator owns the switch. The step is done
when `payment-rails` says so and the counts below agree.
**Rule that does not bend:** enforce is not turned on in a deployment while report only still logs calls
made for a person's session. Such a call is refused under enforce.

1. **Report only.** Set ingress's `approvals` setting to `report_only` (and `environment`, which it needs).
   Nothing is refused. For each command that would be refused, ingress writes one log line (target
   `aledgent_ingress::approval`, with `would_refuse`, the command kind and how the caller proved who it is).
   There is no counter yet: count those lines, and join them to the access log to see which caller they came
   from (the line itself does not name the caller). Callers that are exempt (an issued secret, a configured
   token) are not logged.
2. **`payment-rails` moves.** For each person's session call it makes, it asks for the digest first, shows the
   person the command, has the device approve it, then sends the command with the approval (the page
   "Approving a command", under Integrate). Calls it makes with its issued secret or configured token do not move.
3. **Watch.** The `would_refuse` lines for `payment-rails`' sessions fall to zero (found by joining them to
   the access log), and the refusals ingress would have made fall with them. Anything that remains is a caller that was not expected: find it before going on.
4. **Enforce.** Only when the `would_refuse` lines for sessions have been zero over the period the owner chose. Set `approvals` to
   `enforce`. Keep watching `ERR-CommandApprovalRequired` and `ERR-CredentialInvalid` for a day.
5. **Back.** Set `approvals` back to `report_only`. It takes effect on restart of ingress. Nothing is lost:
   a command taken in under enforce stays taken in.
