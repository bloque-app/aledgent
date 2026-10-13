// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
/** Prefixes a site-absolute path ("/start/getting-started/") with the configured base. */
export function withBase(path: string): string {
	if (!path.startsWith('/')) return path;
	const base = import.meta.env.BASE_URL.replace(/\/+$/, '');
	return `${base}${path}`;
}
