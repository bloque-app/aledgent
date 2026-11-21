// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// specsite's options, in one place: astro.config.mjs publishes the documents with them, and
// lib/id-links.mjs links the guide pages' identifiers to the same definitions.
export const specsiteOptions = {
	root: '..',
	documents: [
		{ file: 'SPEC.md', slug: 'spec', title: 'Specification', split: 'h2',
			defines: ['REQ', 'INV', 'NFR', 'OQ', 'DEF', 'ERR', 'US', 'AC', 'CTR', 'CND'] },
		{ file: 'PLAN.md', slug: 'plan', title: 'Plan', defines: ['DEC', 'F', 'M', 'RISK', 'DEP', 'T'] },
		{ file: 'OVERVIEW.md', slug: 'overview', title: 'Overview', defines: false },
	],
	ids: { prefixes: ['REQ', 'INV', 'NFR', 'OQ', 'DEF', 'DEC', 'ERR', 'US', 'AC', 'CTR', 'CND', 'T', 'F', 'M', 'RISK', 'DEP'] },
	check: 'error',
	sidebar: { label: 'Specification' },
};

/** The site ids specsite publishes (its pages are linked by specsite itself). */
export const specsiteSlugs = specsiteOptions.documents.map((d) => d.slug);
