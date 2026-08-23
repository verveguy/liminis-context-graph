#!/usr/bin/env node
/**
 * Generate the Starlight content collection from the repository's own docs/.
 *
 * docs/*.md is not moved and not modified. It is linked by path from the
 * README, from ADRs, and from `scripts/generate-docs-llms-full.sh`, which
 * concatenates these exact files in a fixed order into a bundle CI checks
 * bitwise. Rewriting them for a site would break all three at once.
 *
 * What this does to each page:
 *
 *   - Replaces the Jekyll front matter with Starlight's. `layout:` was Jekyll's
 *     and has no meaning here; `title:` is carried across where present and
 *     taken from the H1 where it is not.
 *   - Drops the body's H1, since Starlight renders the frontmatter title as the
 *     page heading and two headings is one too many.
 *   - Resolves the two Liquid variables these pages use. `{{ site.version }}`
 *     comes from Cargo.toml's [workspace.package], which is now the only place
 *     a version is written down — there is no second copy to drift from it.
 *
 * Generated as .md unless the page carries a ```c4 fence, in which case .mdx.
 * The fence-to-island swap emits JSX, which Astro only evaluates in MDX — but
 * MDX also reads a bare `{` as the start of an expression, and these reference
 * pages are full of braces. Paying that cost only on the pages that need it
 * keeps a stray brace in a config example from failing the build.
 */

import { readFileSync, writeFileSync, readdirSync, mkdirSync, rmSync, copyFileSync, existsSync } from 'node:fs'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

import { ORDER } from '../src/pages-order.mjs'

const SITE = dirname(dirname(fileURLToPath(import.meta.url)))
const REPO = dirname(SITE)
const SOURCE = join(REPO, 'docs')
// Generated pages live in their own subdirectory so this script can clear it
// wholesale without touching the hand-authored homepage.
// Flat, deliberately. Every page keeps the URL the Jekyll site gave it —
// /liminis-context-graph/ontology/ — because the README, both llms bundles and
// anything else linking here already use those addresses. A tidier /guide/
// prefix would have broken all of them.
//
// Everything in this directory is generated, so it can be cleared wholesale.
const OUT = join(SITE, 'src/content/docs')

export function workspaceVersion() {
  const cargo = readFileSync(join(REPO, 'Cargo.toml'), 'utf8')
  const section = cargo.split('[workspace.package]')[1]
  const version = section && /^version\s*=\s*"([^"]+)"/m.exec(section)
  if (!version) throw new Error('no [workspace.package] version in Cargo.toml')
  return version[1]
}

const REPOSITORY = 'verveguy/liminis-context-graph'
const GITHUB_BLOB = `https://github.com/${REPOSITORY}/blob/main/docs`

const yaml = (s) => `"${s.replace(/"/g, '\\"')}"`

/**
 * Rewrite the pages' links to each other.
 *
 * They are written as `getting-started.md` — correct on GitHub, and correct
 * under Jekyll, whose jekyll-relative-links plugin rewrote them (ADR-0295).
 * Nothing does that here, so a `.md` link would 404 on the site while
 * continuing to work in the repository. Rewritten to routes rather than
 * "fixed" in the source, which would break the GitHub reading of the same file.
 *
 * Relative, not absolute: a version built at /liminis-context-graph/v0.13.4/
 * and the same page built at the root must both resolve, and only a relative
 * link does that without knowing which build it is in.
 */
function rewriteLinks(body, { fromRoot }) {
  const prefix = fromRoot ? './' : '../'
  return body.replace(/\]\((?!https?:|#|\/)([^)\s]+?)\.md(#[^)]*)?\)/g, (whole, target, anchor = '') => {
    // The decision records are not on this site; they redirect to GitHub, and
    // linking there directly saves the reader a bounce.
    if (target.startsWith('adr/')) return `](${GITHUB_BLOB}/${target}.md${anchor})`
    return `](${prefix}${target}/${anchor})`
  })
}

/** Jekyll's own rule: front matter only when the very first line is `---`. */
function splitFrontMatter(text) {
  if (!text.startsWith('---\n')) return { front: '', body: text }
  const end = text.indexOf('\n---\n', 3)
  if (end === -1) return { front: '', body: text }
  return { front: text.slice(4, end), body: text.slice(end + 5) }
}

const version = workspaceVersion()

rmSync(OUT, { recursive: true, force: true })
mkdirSync(OUT, { recursive: true })

// Top level only. adr/ is not part of the site (redirected to GitHub — see
// astro.config.mjs), and history/, spikes/ and examples/ are internal.
const pages = readdirSync(SOURCE, { withFileTypes: true })
  .filter((e) => e.isFile() && e.name.endsWith('.md'))
  .map((e) => e.name)

if (pages.length === 0) {
  console.error(`No markdown found in ${SOURCE}`)
  process.exit(1)
}

let written = 0
for (const file of pages) {
  const slug = file.replace(/\.md$/, '')
  const raw = readFileSync(join(SOURCE, file), 'utf8')
  const { front, body: rawBody } = splitFrontMatter(raw)

  const body = rewriteLinks(
    rawBody
      .replace(/\{\{ *site\.version *\}\}/g, version)
      .replace(/\{\{ *site\.repository *\}\}/g, REPOSITORY),
    { fromRoot: file === 'index.md' },
  )

  const declared = /^title:\s*(.+?)\s*$/m.exec(front)
  const heading = /^#\s+(.+?)\s*$/m.exec(body)
  const title = declared?.[1] ?? heading?.[1]
  if (!title) {
    console.error(`${file}: no title in front matter and no H1 to fall back on`)
    process.exit(1)
  }

  // Strip the H1 only where it is, rather than the first match anywhere: a
  // fenced code block containing a `# comment` line must not be mistaken for
  // the page heading and deleted.
  const withoutHeading = heading
    ? body.slice(0, heading.index) + body.slice(heading.index + heading[0].length)
    : body

  const order = ORDER.indexOf(slug)
  const frontmatter = [
    '---',
    `title: ${yaml(title.replace(/`/g, ''))}`,
    order === -1 ? null : `sidebar:\n  order: ${order + 1}`,
    '---',
  ]
    .filter(Boolean)
    .join('\n')

  const extension = /^```c4\b/m.test(body) ? '.mdx' : '.md'
  writeFileSync(
    join(OUT, slug + extension),
    `${frontmatter}\n${withoutHeading.replace(/^\n+/, '\n')}`,
  )
  written += 1
}

// llms.txt and llms-full.txt are linked from the homepage and are part of what
// the site publishes, but they are plain text rather than pages — served from
// public/ so they keep their exact bytes and their existing URLs.
const PUBLIC = join(SITE, 'public')
mkdirSync(PUBLIC, { recursive: true })
for (const asset of ['llms.txt', 'llms-full.txt']) {
  if (existsSync(join(SOURCE, asset))) copyFileSync(join(SOURCE, asset), join(PUBLIC, asset))
}

console.log(`${written} page(s) generated from ${SOURCE} at v${version}`)
