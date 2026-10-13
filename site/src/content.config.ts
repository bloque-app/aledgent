// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
import { specsiteLoader } from 'specsite/loader';

export const collections = {
	docs: defineCollection({
		loader: specsiteLoader(docsLoader()),
		schema: docsSchema({
			// Two fields the landing hero reads (src/components/Hero.astro); unused elsewhere.
			extend: z.object({
				kicker: z.string().optional(),
				status: z.array(z.string()).optional(),
			}),
		}),
	}),
};
