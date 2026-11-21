// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightOpenAPI, { createOpenAPISidebarGroup } from 'starlight-openapi';
import specsite from 'specsite';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { generateErrors } from './lib/errors.mjs';
import { buildIdIndex } from './lib/id-links.mjs';
import { specsiteOptions } from './lib/specsite-options.mjs';

// GitHub Pages serves a project site under /<repo>/. Both values come from the environment so the
// same build works for a fork, a custom domain (SITE_BASE=/) or a local preview.
const site = process.env.SITE_URL ?? 'https://bloque-app.github.io';
const base = process.env.SITE_BASE ?? '/aledgent/';

// The API reference is generated from ingress's own OpenAPI document, read in place, so it cannot
// drift from the service: services/ingress/openapi.json is generated from ingress's code, and
// ingress's surface test fails when the committed file is stale.
const apiGroup = createOpenAPISidebarGroup();
const operatorGroup = createOpenAPISidebarGroup();

// The reference is split in two, at build time, from that same document: what an integrator calls,
// and the operator's surface (operations, the settlement adapter, metrics). Nothing is committed; the
// two halves are regenerated on every build.
const OPERATOR = (path) => path.startsWith('/v1/ops/') || path.startsWith('/v1/settlement/') || path === '/metrics';
const spec = JSON.parse(readFileSync(new URL('../services/ingress/openapi.json', import.meta.url), 'utf8'));
const half = (keep, title) => ({
	...spec,
	info: { ...spec.info, title },
	paths: Object.fromEntries(Object.entries(spec.paths ?? {}).filter(([path]) => keep(path))),
});
const generated = new URL('./.generated/', import.meta.url);
mkdirSync(generated, { recursive: true });
writeFileSync(new URL('openapi-integrator.json', generated), JSON.stringify(half((p) => !OPERATOR(p), spec.info?.title ?? 'aledgent')));
writeFileSync(new URL('openapi-operator.json', generated), JSON.stringify(half(OPERATOR, `${spec.info?.title ?? 'aledgent'}: operator API`)));

// Where every identifier is defined, for the guide pages' links (lib/id-links.mjs).
writeFileSync(new URL('id-index.json', generated), JSON.stringify(await buildIdIndex(fileURLToPath(new URL('.', import.meta.url)))));

// The error catalogue, one page per ERR-* of SPEC.md §10, generated on every build from §10, this same
// OpenAPI document and src/data/error-actions.md (lib/errors.mjs). It throws, and the build fails, when
// the three disagree. The pages land in a gitignored folder; nothing is committed.
generateErrors({ root: new URL('../', import.meta.url), out: new URL('./src/content/docs/errors/', import.meta.url) });

export default defineConfig({
	site,
	base,
	output: 'static',
	integrations: [
		starlight({
			title: 'aledgent',
			description:
				'An open-source ledger whose book anyone it concerns can check against a public anchor.',
			logo: {
				light: './src/assets/lockup-light.svg',
				dark: './src/assets/lockup-dark.svg',
				alt: 'aledgent by bloque',
				replacesTitle: true,
			},
			favicon: '/favicon.svg',
			// Link previews (Slack, social): one card for every page, served from site/public/og.png.
			head: [
				{ tag: 'meta', attrs: { property: 'og:image', content: `${site}${base}og.png` } },
				{ tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
				{ tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
				{ tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
				{ tag: 'meta', attrs: { name: 'twitter:image', content: `${site}${base}og.png` } },
			],
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/bloque-app/aledgent' }],
			editLink: { baseUrl: 'https://github.com/bloque-app/aledgent/edit/main/site/' },
			// English only for now. Adding a language is `locales` here plus a folder under
			// src/content/docs/<lang>/; the root stays English.
			defaultLocale: 'root',
			locales: { root: { label: 'English', lang: 'en' } },
			customCss: [
				'@fontsource/familjen-grotesk/700.css',
				'@fontsource/geist/400.css',
				'@fontsource/geist/500.css',
				'@fontsource/geist-mono/400.css',
				'./src/styles/theme.css',
			],
			components: {
				Footer: './src/components/Footer.astro',
				Hero: './src/components/Hero.astro',
				MarkdownContent: './src/components/MarkdownContent.astro',
			},
			expressiveCode: {
				themes: ['github-light', 'github-dark'],
				styleOverrides: {
					borderRadius: '18px',
					borderColor: 'var(--sl-color-hairline)',
					codeFontFamily: 'var(--__sl-font-mono)',
					uiFontFamily: 'var(--__sl-font)',
					frames: { shadowColor: 'transparent' },
				},
			},
			plugins: [
				starlightOpenAPI([
					{
						base: 'api',
						schema: './.generated/openapi-integrator.json',
						sidebar: { label: 'API reference', collapsed: false, group: apiGroup, operations: { badges: true, labels: 'path' } },
					},
					{
						base: 'api/operator',
						schema: './.generated/openapi-operator.json',
						sidebar: { label: 'Operator API', collapsed: true, group: operatorGroup, operations: { badges: true, labels: 'path' } },
					},
				]),
				// The specification, plan and overview, rendered from the repo's own Markdown on every build
				// (the .md files are the source of truth; nothing is copied). Ids link to their definition.
				specsite(specsiteOptions),
			],
			sidebar: [
				{ label: 'Start', items: [{ autogenerate: { directory: 'start' } }] },
				{ label: 'Concepts', items: [{ autogenerate: { directory: 'concepts' } }] },
				{
					label: 'Integrate',
					items: [
						{ autogenerate: { directory: 'integrate' } },
						{ label: 'Errors', collapsed: true, items: [{ autogenerate: { directory: 'errors' } }] },
					],
				},
				{ label: 'Operate', items: [{ autogenerate: { directory: 'operate' } }] },
				apiGroup,
				operatorGroup,
			],
		}),
	],
});
