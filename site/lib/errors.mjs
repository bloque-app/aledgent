// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// The error catalogue (F-039, M-3D: "every ERR-* has a page with its cause and the caller's action").
//
// One page per refusal, generated on every build from three sources, none of them copied:
//   - SPEC.md §10: the closed list of conditions (REQ-ER-1), each with its condition text;
//   - services/ingress/openapi.json: the Refusal schema, which carries each condition's HTTP status
//     (x-http-status) and the description ingress serves;
//   - site/src/data/error-actions.md: what the caller does, one section per condition.
// The build fails when they disagree: a condition without an action, an action for a condition §10
// does not name, a condition the OpenAPI document does not carry (or carries with another
// description). That is the check that keeps the catalogue in step with the specification.
//
// The pages are written to src/content/docs/errors/, which is gitignored: nothing generated
// is committed. `node lib/errors.mjs --check` runs the same checks without writing.
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const REPO = 'https://github.com/bloque-app/aledgent';
const ERR = /^ERR-[A-Za-z]+$/;

/** §10's table: `| \`ERR-X\` | condition |`, in order. */
export function specErrors(spec) {
	const start = spec.search(/^## 10\. Errors$/m);
	if (start < 0) throw new Error('errors: SPEC.md has no "## 10. Errors" section');
	const rest = spec.slice(start + 1);
	const end = rest.search(/^## /m);
	const section = end < 0 ? rest : rest.slice(0, end);
	const out = [];
	for (const line of section.split('\n')) {
		const m = line.match(/^\| `(ERR-[A-Za-z]+)` \| (.*) \|$/);
		if (m) out.push({ id: m[1], condition: m[2].trim() });
	}
	if (out.length === 0) throw new Error('errors: SPEC.md §10 lists no ERR-* condition');
	return out;
}

/** The OpenAPI Refusal schema: id → { status, description }. */
export function openapiErrors(openapi) {
	const alts = openapi?.components?.schemas?.Refusal?.oneOf;
	if (!Array.isArray(alts)) throw new Error('errors: openapi.json has no components.schemas.Refusal.oneOf');
	const out = new Map();
	for (const a of alts) {
		const id = a?.properties?.error?.const;
		out.set(id, { status: a['x-http-status'], description: a?.properties?.description?.const });
	}
	return out;
}

/** error-actions.md: `## ERR-X`, then `Seen by:` and `Retry:` lines, then the action. */
export function actions(md) {
	const out = new Map();
	const parts = md.split(/^## /m).slice(1);
	for (const part of parts) {
		const nl = part.indexOf('\n');
		const id = part.slice(0, nl).trim();
		if (!ERR.test(id)) throw new Error(`errors: error-actions.md has a section "${id}" that is not an ERR-* identifier`);
		if (out.has(id)) throw new Error(`errors: error-actions.md has two sections for ${id}`);
		const body = part.slice(nl + 1).trim();
		const seen = body.match(/^Seen by: (.+)$/m);
		const retry = body.match(/^Retry: (.+)$/m);
		if (!seen || !retry) throw new Error(`errors: ${id} in error-actions.md needs a "Seen by:" and a "Retry:" line`);
		const action = body.replace(/^Seen by: .+$/m, '').replace(/^Retry: .+$/m, '').trim();
		if (!action) throw new Error(`errors: ${id} in error-actions.md says nothing about what to do`);
		out.set(id, { seen: seen[1].trim(), retry: retry[1].trim(), action });
	}
	return out;
}

/** Join the three sources, or throw with every disagreement listed. */
export function catalogue({ spec, openapi, actionsMd }) {
	const fromSpec = specErrors(spec);
	const fromApi = openapiErrors(openapi);
	const fromActions = actions(actionsMd);
	const problems = [];
	const ids = new Set(fromSpec.map((e) => e.id));
	for (const e of fromSpec) {
		const api = fromApi.get(e.id);
		if (!api) problems.push(`${e.id} is in SPEC.md §10 but not in the OpenAPI document's Refusal schema`);
		else if (api.description !== e.condition)
			problems.push(`${e.id}: the OpenAPI description differs from SPEC.md §10's condition`);
		else if (!Number.isInteger(api.status)) problems.push(`${e.id} has no x-http-status in the OpenAPI document`);
		if (!fromActions.has(e.id)) problems.push(`${e.id} is in SPEC.md §10 but has no section in site/src/data/error-actions.md`);
	}
	for (const id of fromApi.keys()) if (!ids.has(id)) problems.push(`${id} is in the OpenAPI document but not in SPEC.md §10`);
	for (const id of fromActions.keys()) if (!ids.has(id)) problems.push(`${id} has a section in error-actions.md but is not in SPEC.md §10`);
	if (problems.length) throw new Error(`errors: the error catalogue is out of step:\n  - ${problems.join('\n  - ')}`);
	return fromSpec.map((e, i) => ({ ...e, order: i + 1, ...fromApi.get(e.id), ...fromActions.get(e.id) }));
}

const slug = (id) => id.toLowerCase();
// Front matter takes plain text: drop the code ticks of identifiers, and quote for YAML.
const plain = (s) => JSON.stringify(s.replace(/`/g, ''));

function page(e) {
	const body = JSON.stringify({ error: e.id, description: e.description }, null, 2);
	return `---
title: ${e.id}
description: ${plain(e.condition)}
editUrl: ${REPO}/edit/main/site/src/data/error-actions.md
sidebar:
  order: ${e.order}
---

<!-- Generated by site/lib/errors.mjs from SPEC.md §10, the OpenAPI document and error-actions.md. Do not edit. -->

**HTTP ${e.status}.** ${e.condition}

- **Seen by:** ${e.seen}
- **Retry:** ${e.retry}

## What to do

${e.action}

## The reply

\`\`\`json
${body}
\`\`\`

The body is the same everywhere the refusal is returned. A refusal names no field and quotes nothing you
sent. For a command acknowledged at \`Received\`, the same condition arrives later, in the operation's
\`error\` (\`GET /v1/operations/{reference}\`) and in its notification.

Defined in the specification, [§10 Errors](../../spec/10-errors/#${slug(e.id)}).
`;
}

function index(list) {
	const rows = list
		.map((e) => `| [\`${e.id}\`](./${slug(e.id)}/) | ${e.status} | ${e.retry} |`)
		.join('\n');
	return `---
title: Errors
description: Every refusal aledgent can return, with its HTTP status, whether to retry, and what to do.
editUrl: ${REPO}/edit/main/site/src/data/error-actions.md
sidebar:
  order: 0
  label: All errors
---

<!-- Generated by site/lib/errors.mjs. Do not edit. -->

A refusal always names one condition from a closed list (\`REQ-ER-1\`): nothing outside it is ever returned,
and no success reply carries an error. Each has a page with its cause and what to do. The list is generated
from the specification's §10 and ingress's OpenAPI document at every build, so it cannot drift from them.

A refusal reaches you in one of two ways. A command answered with its decision gets it as the HTTP reply. A
command acknowledged at \`Received\` (a transfer, a reservation, a withdrawal) is accepted with \`202\`, and
a refusal decided later shows as the operation's \`Rejected\` state, with the condition in \`error\`. See
[The operation lifecycle](../concepts/operation-lifecycle/).

Three rules cover most of the table:

- **\`503 ERR-DecisionUnavailable\` is not a failure.** Retry with the same client key after \`Retry-After\`.
- **\`500 ERR-Internal\` is safe to retry** with the same client key.
- **Everything else is a 4xx about the request or its authority.** Repeating it unchanged gets the same answer.

| Condition | HTTP | Retry |
|---|---|---|
${rows}
`;
}

/** Check the sources and, unless `check`, write the pages. Returns the number of conditions. */
export function generateErrors({ root, out, check = false }) {
	const read = (p) => readFileSync(new URL(p, root), 'utf8');
	const list = catalogue({
		spec: read('SPEC.md'),
		openapi: JSON.parse(read('services/ingress/openapi.json')),
		actionsMd: read('site/src/data/error-actions.md'),
	});
	if (!check) {
		rmSync(out, { recursive: true, force: true });
		mkdirSync(out, { recursive: true });
		writeFileSync(new URL('index.md', out), index(list));
		for (const e of list) writeFileSync(new URL(`${slug(e.id)}.md`, out), page(e));
	}
	return list.length;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const root = new URL('../../', import.meta.url);
	const n = generateErrors({ root, out: new URL('../src/content/docs/errors/', import.meta.url), check: process.argv.includes('--check') });
	console.log(`errors: ${n} conditions, SPEC.md §10, the OpenAPI document and error-actions.md agree`);
}
