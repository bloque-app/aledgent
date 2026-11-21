// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// The built site's internal links, checked (F-039, M-3D: "the site's strict checks pass"). Every link from a
// page to another page of the site must reach a page that exists, and its fragment, if any, an element with
// that id. Runs after `astro build` over dist/; exits non-zero with every broken link listed.
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = fileURLToPath(new URL('../dist/', import.meta.url));
const base = (process.env.SITE_BASE ?? '/aledgent/').replace(/\/?$/, '/');

const pages = [];
(function walk(dir) {
	for (const name of readdirSync(dir)) {
		const p = join(dir, name);
		if (statSync(p).isDirectory()) {
			if (name !== '_astro' && name !== 'pagefind') walk(p);
		} else if (name.endsWith('.html')) pages.push(p);
	}
})(dist);

const idsOf = new Map();
function ids(file) {
	if (!idsOf.has(file)) {
		const html = readFileSync(file, 'utf8');
		idsOf.set(file, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1])));
	}
	return idsOf.get(file);
}

/** The built file a site path is served from, or undefined. */
function target(path) {
	const rel = decodeURIComponent(path.slice(base.length));
	for (const c of [join(dist, rel), join(dist, rel, 'index.html'), join(dist, `${rel}.html`)]) {
		if (existsSync(c) && statSync(c).isFile()) return c;
	}
	return undefined;
}

const broken = [];
for (const page of pages) {
	const html = readFileSync(page, 'utf8');
	const pageUrl = `${base}${relative(dist, page).split(sep).join('/').replace(/index\.html$/, '')}`;
	for (const m of html.matchAll(/<a\s[^>]*href="([^"]+)"/g)) {
		const href = m[1].replace(/&amp;/g, '&');
		if (/^[a-z]+:/i.test(href) || href.startsWith('//')) continue;
		const url = new URL(href, `https://site.invalid${pageUrl}`);
		if (!url.pathname.startsWith(base)) continue;
		const file = target(url.pathname);
		const frag = decodeURIComponent(url.hash.slice(1));
		if (!file) broken.push(`${pageUrl}: ${href} (no such page)`);
		else if (frag && !ids(file).has(frag)) broken.push(`${pageUrl}: ${href} (no #${frag} on that page)`);
	}
}

if (broken.length) {
	console.error(`check-links: ${broken.length} broken internal link(s):\n  ${[...new Set(broken)].join('\n  ')}`);
	process.exit(1);
}
console.log(`check-links: ${pages.length} pages, every internal link resolves`);
