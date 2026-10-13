<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# aledgent docs site

The public documentation site for aledgent, built with [Starlight](https://starlight.astro.build)
(Astro's documentation framework) and themed with Bloque's design system. It is published to GitHub
Pages from `main` once the repository is public (`.github/workflows/docs.yml`). Before the first
deploy, set the repository's Pages source to **GitHub Actions** (Settings › Pages).

This is the public site. The repository's `docs/` folder (licensing, the marks, code history) is
not part of it.

## Run and build

Node 22.12 or later.

```sh
cd site
npm ci
npm run dev       # http://localhost:4321/aledgent/
npm run build     # static output in site/dist/
npm run preview   # serves site/dist/
```

The site is served under a base path. Two environment variables set it, so the same build works for
a fork, a custom domain or a local preview:

| Variable | Default | |
| --- | --- | --- |
| `SITE_URL` | `https://bloque-app.github.io` | The origin, for canonical URLs and the sitemap |
| `SITE_BASE` | `/aledgent/` | The path the site is served under. `/` for a custom domain |

Links inside the site's own components go through `src/lib/url.ts` (`withBase`). Links in Markdown
pages are relative (`../../concepts/operation-lifecycle/`), so they work under any base.

## Layout

```
site/
  astro.config.mjs          Starlight, the sidebar, the OpenAPI plugin, fonts, component overrides
  src/content.config.ts     The docs collection; adds `kicker` and `status` for the landing hero
  src/content/docs/         Pages. Folder = sidebar group (start/, concepts/, operate/)
  src/components/           Hero (override), Footer (wraps Starlight's), Concept, Bounds
  src/styles/theme.css      Bloque's tokens mapped onto Starlight's custom properties
  screenshots/              Light and dark captures of the prototype, for review
```

English only for now. The root locale is English (`locales.root` in `astro.config.mjs`). Adding a
language is one more entry there and a folder `src/content/docs/<lang>/` mirroring the root.

## API reference

`starlight-openapi` generates the API reference (`/api/`) at build time from
`../services/ingress/openapi.json`, read in place and never copied. That file is generated from
ingress's code, and ingress's surface test fails when the committed copy is stale
(`UPDATE_OPENAPI=1 cargo test -p ingress --test surface` regenerates it), so the reference follows
the service. The docs workflow runs on any change to it.

The document declares no `tags`, so every route is listed under one "Operations" group, by path.
It declares no `servers` either, so the generated snippets use `https://example.com`.

## Theme map

Starlight's grey scale runs from `--sl-color-white` (strongest text) to `--sl-color-black` (page
ground), and it is inverted between the themes. `theme.css` sets both.

| Bloque token | Value | Starlight variable, light | Starlight variable, dark |
| --- | --- | --- | --- |
| Ink | `#0d0c17` | `--sl-color-white`, `--sl-color-gray-1` (headings, strong text) | `--sl-color-black`, `--sl-color-bg` (ground) |
| Paper, raised/page | `#fbfaff` | `--sl-color-black`, `--sl-color-bg`, `-bg-nav`, `-bg-sidebar` | `--sl-color-white` (headings) |
| Paper, sunken | `#f4f2fa` | `--sl-color-gray-7`, `--sl-color-bg-inline-code`, active sidebar entry | |
| Border | `#e9e6f2` | `--sl-color-gray-6`, `--sl-color-hairline`, `-hairline-light` | `--sl-color-gray-1` (text) |
| Border, strong | `#d6d2e3` | `--sl-color-gray-5`, `--sl-color-hairline-shade` | `--sl-color-gray-2` (body text) |
| Text, strong secondary | `#38354a` | `--sl-color-gray-2` (body text) | `--sl-color-gray-4`, `--sl-color-hairline-shade` |
| Text, secondary | `#6d6982` | `--sl-color-gray-3` | (not used: 3.7:1 on ink) |
| Text, tertiary | `#9a96ad` | `--sl-color-gray-4` (borders and icons only, never text) | `--sl-color-gray-3` (secondary text) |
| Ink surface | `#17152a` | | `--sl-color-gray-6`, `--sl-color-bg-inline-code`, cards |
| Ink borders | `#2a2838` | | `--sl-color-gray-5`, `--sl-color-hairline` |
| Bloque Violet | `#6a3cff` | `--sl-color-accent`, `-accent-high`, `-text-accent`; primary buttons | |
| Violet-100 | `#ece6ff` | `--sl-color-accent-low` (accent tint, note asides) | |
| Lilac | `#a78bfa` | | `--sl-color-accent`, `-accent-high`, `-text-accent` |
| White on accent | `#ffffff` | `--sl-color-text-invert` | `--sl-color-text-invert` is ink on lilac |

One value is derived, not a named token: `#241d45`, the lilac tint on ink (`--sl-color-accent-low`
in dark).

Type: headings and the site title in **Familjen Grotesk** 700, tracking −0.025em (h1 −0.04em);
body in **Geist** 400/500 at a line height of 1.6; code in **Geist Mono**. All three are self-hosted
from `@fontsource` packages, with system fallbacks.

Shape: buttons, badges and sidebar entries are pills (999px); cards 28px; code blocks, inputs and
asides 18px. Shadows are ink-tinted and used only on light-theme cards. No gradients. Asides are a
tinted background with a 1px border, not Starlight's left rule.

What stays stock: Starlight's layout, header, search (Pagefind), sidebar and table of contents;
Expressive Code's `github-light`/`github-dark` syntax colours; the API reference's per-method badge
colours (GET, POST). The plugin's panels, example bodies and "required" marker are restyled in
`theme.css` (selectors prefixed `html:root`, because its scoped styles load after this file).

### Contrast (WCAG 2.x, normal text needs 4.5:1)

| Pair | Ratio |
| --- | --- |
| Ink on paper | 18.7 |
| `#38354a` on paper (body text, light) | 11.4 |
| `#6d6982` on paper / on sunken | 5.1 / 4.7 |
| Violet on paper / on sunken / on violet-100 | 5.5 / 5.1 / 4.7 |
| White on violet (primary button) | 5.7 |
| `#e9e6f2` on ink / `#d6d2e3` on ink surface | 15.8 / 12.1 |
| `#9a96ad` on ink / on ink surface | 6.8 / 6.2 |
| Lilac on ink / on ink surface / on `#2a2838` | 7.1 / 6.6 / 5.3 |
| Ink on lilac (accent fill, dark) | 7.1 |
| `#9a96ad` on paper | **2.75: fails**, so it is never text in light |
| `#6d6982` on ink | **3.7: fails**, so it is not used in dark |
