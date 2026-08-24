#!/usr/bin/env node
/**
 * Check that every internal link in the built site resolves.
 *
 * Replaces the htmlproofer pass the Jekyll site ran in CI (docs-drift.yml). The
 * failure it exists to catch is specific and easy to reintroduce: these pages
 * link to each other as `configuration.md`, which is correct on GitHub and
 * rewritten for the site by sync-docs.mjs. A link that slips through unrewritten
 * looks fine in the repository and 404s on the site.
 *
 * Checks relative links as well as root-relative ones, because the rewrite
 * produces relative links (`../configuration/`) — checking only `/`-prefixed
 * URLs would have missed precisely the thing this is here for. `srcset` counts
 * too: the dark-mode SVG of every diagram is referenced that way and nothing
 * else would notice it going missing.
 *
 * External links are not checked, exactly as before — a docs build that fails
 * because someone else's server is down is a build that gets ignored.
 *
 *     node scripts/check-links.mjs [dist-dir]
 */

import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs'
import { join, dirname, resolve, posix } from 'node:path'
import { fileURLToPath } from 'node:url'

const SITE = dirname(dirname(fileURLToPath(import.meta.url)))
const DIST = resolve(process.argv[2] ?? join(SITE, 'dist'))

// The base this build was made with. Taken from the environment rather than
// assumed to be one path segment: a versioned build is served from
// /liminis-context-graph/vX.Y.Z/, and stripping only the first segment would
// leave the version in the path and report every link as broken.
const BASE = (process.env.DOCS_BASE ?? '/liminis-context-graph').replace(/\/$/, '')

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

/** A URL resolves if it is a file, or a directory Astro gave an index.html. */
function resolves(pathname) {
  // Root-relative but outside this build — versions.json, and the fixed-path
  // fetch the version switcher makes. Published by the accumulator, not by this
  // build, so not this check's business.
  if (!pathname.startsWith(BASE + '/') && pathname !== BASE) return true
  const target = join(DIST, pathname.slice(BASE.length))
  if (existsSync(target) && statSync(target).isFile()) return true
  return existsSync(join(target.replace(/\/$/, ''), 'index.html'))
}

/** The URL a built file is served at, so relative links can be resolved. */
function pageUrl(file) {
  const rel = file.slice(DIST.length).replace(/\\/g, '/')
  return BASE + rel.replace(/index\.html$/, '').replace(/\.html$/, '')
}

const broken = []
let checked = 0

for (const file of htmlFiles(DIST)) {
  const html = readFileSync(file, 'utf8')
  const from = pageUrl(file)

  // srcset carries the dark-mode SVG of every rendered diagram. It can hold a
  // descriptor list, so take the URL of each candidate rather than the field.
  const urls = [
    ...[...html.matchAll(/(?:href|src)="([^"]+)"/g)].map((m) => m[1]),
    ...[...html.matchAll(/srcset="([^"]+)"/g)].flatMap((m) =>
      m[1].split(',').map((candidate) => candidate.trim().split(/\s+/)[0]),
    ),
  ]

  for (const raw of urls) {
    if (!raw || /^(?:[a-z]+:|\/\/|#|mailto:|data:)/i.test(raw)) continue
    const [path] = raw.split(/[#?]/)
    if (!path) continue
    const absolute = path.startsWith('/') ? path : posix.resolve(from, path)
    checked += 1
    if (!resolves(absolute)) broken.push(`${file.slice(DIST.length + 1)} -> ${raw}`)
  }
}

if (broken.length > 0) {
  console.error(`${broken.length} broken internal link(s):`)
  for (const b of [...new Set(broken)].sort()) console.error(`  ${b}`)
  process.exit(1)
}

console.log(`${checked} internal link(s) checked, all resolve.`)
