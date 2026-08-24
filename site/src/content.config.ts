// Required by Astro 5: without this the docs/ collection is never loaded and the
// build quietly produces a site with only a 404 page — no error, just nothing.
import { defineCollection } from 'astro:content'
import { docsLoader } from '@astrojs/starlight/loaders'
import { docsSchema } from '@astrojs/starlight/schema'

export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
}
