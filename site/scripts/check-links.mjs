#!/usr/bin/env node
/**
 * Check that every internal link in the built site resolves to a page.
 *
 * Replaces the htmlproofer pass the Jekyll site ran in CI (docs-drift.yml). The
 * failure it exists to catch is specific and easy to reintroduce: these pages
 * link to each other as `configuration.md`, which is correct on GitHub and
 * rewritten for the site by sync-docs.mjs. A page that slips through unrewritten
 * looks fine in the repository and 404s on the site.
 *
 * External links are not checked, exactly as before — a docs build that fails
 * because someone else's server is down is a build that gets ignored.
 *
 *     node scripts/check-links.mjs [dist-dir]
 */

import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs'
import { join, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const SITE = dirname(dirname(fileURLToPath(import.meta.url)))
const DIST = resolve(process.argv[2] ?? join(SITE, 'dist'))

if (!existsSync(DIST)) {
  console.error(`No build to check at ${DIST}. Run \`pnpm build\` first.`)
  process.exit(1)
}

function* htmlFiles(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name)
    if (entry.isDirectory()) yield* htmlFiles(path)
    else if (entry.name.endsWith('.html')) yield path
  }
}

// The base this build was made with. Taken from the environment rather than
// assumed to be one path segment: a versioned build is served from
// /liminis-context-graph/vX.Y.Z/, and stripping only the first segment would
// leave the version in the path and report every link as broken.
const BASE = (process.env.DOCS_BASE ?? '/liminis-context-graph').replace(/\/$/, '')

/** A URL resolves if it is a file, or a directory Astro gave an index.html. */
function resolves(pathname) {
  if (!pathname.startsWith(BASE + '/') && pathname !== BASE) {
    // Root-relative but outside this build — versions.json, and the fixed-path
    // fetch the version switcher makes. Published by the accumulator, not by
    // this build, so it is not this check's business.
    return true
  }
  const target = join(DIST, pathname.slice(BASE.length))
  if (existsSync(target) && statSync(target).isFile()) return true
  return existsSync(join(target.replace(/\/$/, ''), 'index.html'))
}

const broken = []
let checked = 0

for (const file of htmlFiles(DIST)) {
  const html = readFileSync(file, 'utf8')
  for (const match of html.matchAll(/(?:href|src)="(\/[^"#?]*)/g)) {
    const url = match[1]
    checked += 1
    if (!resolves(url)) broken.push(`${file.slice(DIST.length + 1)} -> ${url}`)
  }
}

if (broken.length > 0) {
  console.error(`${broken.length} broken internal link(s):`)
  for (const b of [...new Set(broken)].sort()) console.error(`  ${b}`)
  process.exit(1)
}

console.log(`${checked} internal link(s) checked, all resolve.`)
