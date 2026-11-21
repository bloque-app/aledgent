// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Identifiers in the guide pages (F-039: "each page citing the identifiers it explains").
//
// specsite links identifiers inside the documents it publishes (SPEC.md, PLAN.md, OVERVIEW.md). The guide
// pages are ordinary Starlight pages, so this does the same for them, with specsite's own index of
// definitions: every identifier written in inline code (`REQ-CS-7`) becomes a link to where it is defined,
// and an `ERR-*` to its page in the error catalogue. An identifier nothing defines fails the build, as
// specsite's `check: 'error'` does for the documents.
//
// Two halves: `buildIdIndex` runs in astro.config.mjs (plain Node, at config time) and writes the index to
// .generated/id-index.json; `linkIds` runs in src/components/MarkdownContent.astro, from that file.
import { fromHtml } from 'hast-util-from-html';
import { toHtml } from 'hast-util-to-html';
import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkGfm from 'remark-gfm';
import remarkRehype from 'remark-rehype';
import rehypeRaw from 'rehype-raw';
import { createIdMatcher, linkMentions, loadDocuments, processPages, resolveOptions } from 'specsite/core';
import { specsiteOptions } from './specsite-options.mjs';

/** id → site path (no base), from the documents specsite publishes. `siteDir` is the Astro project root. */
export async function buildIdIndex(siteDir) {
	const r = resolveOptions(specsiteOptions, siteDir);
	const docs = loadDocuments(r);
	const md = unified().use(remarkParse).use(remarkGfm).use(remarkRehype, { allowDangerousHtml: true }).use(rehypeRaw);
	const rendered = [];
	for (const spec of docs.flatMap((d) => d.pages)) {
		const tree = await md.run(md.parse(spec.markdown));
		rendered.push({ spec, tree });
	}
	const { index } = processPages(rendered, {
		ids: r.ids,
		rules: r.rules,
		linkText: false,
		repo: r.repo,
		pageUrl: (p) => `/${p.id}/`,
		readOnly: () => true,
	});
	const paths = {};
	for (const [id, def] of index) paths[id] = `${def.page.id}/${def.anchor ? `#${def.anchor}` : ''}`;
	return paths;
}

const ids = createIdMatcher({ prefixes: specsiteOptions.ids.prefixes });

/** Link the identifiers in one page's rendered content. Throws on an identifier nothing defines. */
export function linkIds(html, { base, page, index }) {
	const paths = new Map(Object.entries(index));
	const root = base.endsWith('/') ? base : `${base}/`;
	const tree = fromHtml(html, { fragment: true });
	const unknown = new Set();
	linkMentions(
		tree,
		ids,
		(id) => {
			if (id.startsWith('ERR-')) return paths.has(id) ? `${root}errors/${id.toLowerCase()}/` : undefined;
			const p = paths.get(id);
			return p === undefined ? undefined : `${root}${p}`;
		},
		{ linkText: false, unknown: (id) => unknown.add(id) },
	);
	if (unknown.size) {
		throw new Error(`id-links: ${page} cites ${[...unknown].join(', ')}, which nothing in SPEC.md or PLAN.md defines`);
	}
	return toHtml(tree);
}
