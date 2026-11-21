// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// No published page names an operator's accounts or internal hosts (PLAN.md M-3D). Runs after `astro build`
// over dist/: the text of every page, and every link and source it carries, is checked against the shapes in
// lib/public-denylist.json, and against an operator's own literal names when they are passed in
// PUBLIC_DENYLIST_PATTERNS (one regular expression per line, kept out of this public repository). Exits
// non-zero with every match listed.
//
// `--self-test` checks the denylist itself: every pattern compiles, matches each of its examples and none of
// its counter-examples. It reads no page.
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
const dist = fileURLToPath(new URL('../dist/', import.meta.url));
const { patterns } = JSON.parse(readFileSync(join(here, 'public-denylist.json'), 'utf8'));

const compiled = patterns.map((p) => ({ ...p, re: new RegExp(p.regex, `${(p.flags ?? '').replace('g', '')}g`) }));

function selfTest() {
	const faults = [];
	const ids = new Set();
	for (const p of compiled) {
		if (ids.has(p.id)) faults.push(`${p.id}: the id is used twice`);
		ids.add(p.id);
		if (!p.why) faults.push(`${p.id}: says nothing about why`);
		if (!p.examples?.length) faults.push(`${p.id}: has no example`);
		for (const e of p.examples ?? []) if (!new RegExp(p.re.source, p.re.flags.replace('g', '')).test(e)) faults.push(`${p.id}: misses its example "${e}"`);
		for (const e of p.not ?? []) if (new RegExp(p.re.source, p.re.flags.replace('g', '')).test(e)) faults.push(`${p.id}: matches its counter-example "${e}"`);
	}
	if (faults.length) {
		console.error(`check-public --self-test: ${faults.length} fault(s) in lib/public-denylist.json:\n  ${faults.join('\n  ')}`);
		process.exit(1);
	}
	console.log(`check-public --self-test: ${compiled.length} patterns, every example matched and no counter-example`);
}

/** The operator's own patterns, from the environment. Reported by number, never by their text. */
function extra() {
	const raw = process.env.PUBLIC_DENYLIST_PATTERNS ?? '';
	return raw
		.split('\n')
		.map((l) => l.trim())
		.filter((l) => l && !l.startsWith('#'))
		.map((l, i) => {
			try {
				return { id: `operator-pattern-${i + 1}`, re: new RegExp(l, 'gi'), private: true };
			} catch {
				console.error(`check-public: PUBLIC_DENYLIST_PATTERNS line ${i + 1} is not a regular expression`);
				process.exit(1);
			}
		});
}

/** What a reader of a page sees or follows: its text, and every href, src and content attribute. */
function published(html) {
	const attrs = [...html.matchAll(/\s(?:href|src|content|title|alt)="([^"]*)"/g)].map((m) => m[1]);
	const text = html
		.replace(/<script\b[\s\S]*?<\/script>/gi, ' ')
		.replace(/<style\b[\s\S]*?<\/style>/gi, ' ')
		.replace(/<[^>]+>/g, ' ');
	const decode = (s) =>
		s
			.replace(/&lt;/g, '<')
			.replace(/&gt;/g, '>')
			.replace(/&quot;/g, '"')
			.replace(/&#39;|&#x27;/g, "'")
			.replace(/&#x2F;|&#47;/g, '/')
			.replace(/&amp;/g, '&');
	return [decode(text), ...attrs.map(decode)].join('\n');
}

function check() {
	if (!existsSync(dist)) {
		console.error('check-public: dist/ does not exist; run astro build first');
		process.exit(1);
	}
	const pages = [];
	(function walk(dir) {
		for (const name of readdirSync(dir)) {
			const p = join(dir, name);
			if (statSync(p).isDirectory()) {
				if (name !== '_astro' && name !== 'pagefind') walk(p);
			} else if (name.endsWith('.html')) pages.push(p);
		}
	})(dist);
	const all = [...compiled, ...extra()];
	const found = [];
	for (const page of pages) {
		const body = published(readFileSync(page, 'utf8'));
		const at = `/${relative(dist, page).split(sep).join('/')}`;
		for (const p of all) {
			for (const m of body.matchAll(p.re)) {
				found.push(`${at}: ${p.id}${p.private ? '' : ` (${p.why})`}: "${m[0]}"`);
			}
		}
	}
	if (found.length) {
		console.error(`check-public: ${found.length} match(es) of what no public page may name:\n  ${[...new Set(found)].join('\n  ')}`);
		process.exit(1);
	}
	const n = all.length - compiled.length;
	console.log(`check-public: ${pages.length} pages, ${compiled.length} shapes${n ? ` and ${n} operator patterns` : ''}, nothing found`);
}

if (process.argv.includes('--self-test')) selfTest();
else check();
