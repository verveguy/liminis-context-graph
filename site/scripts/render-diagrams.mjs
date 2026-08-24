#!/usr/bin/env node
/**
 * Render every ```c4 fence in docs/ to an SVG, and keep a <picture> beside the
 * fence pointing at it.
 *
 * This is the GitHub half of the single-source-of-truth idea. On the docs site a
 * remark plugin swaps the fence for a live island and strips the <img>; on
 * github.com there is no build step, so the committed SVG is what a reader sees.
 * Same file, two renderings, one source.
 *
 * The rendering is done by `@liminis/diagrams/server` — the package's own
 * published entry point. Two things follow from that which are worth having:
 * the docs build exercises `/server` on every commit, which nothing else in CI
 * does, and the images cannot drift from what the library actually produces,
 * because they are produced by it.
 *
 *     node scripts/render-diagrams.mjs           write SVGs and update <img> tags
 *     node scripts/render-diagrams.mjs --check   fail if anything is out of date
 *
 * `--check` is what CI runs: a committed SVG that no longer matches its fence is
 * a stale picture on GitHub, and stale documentation is worse than none.
 */

import { readFileSync, writeFileSync, mkdirSync, existsSync, readdirSync, rmSync } from 'node:fs'
import { join, dirname, basename } from 'node:path'
import { fileURLToPath } from 'node:url'
import { renderC4DiagramToSVG } from '@liminis/diagrams/server'

const SITE = dirname(dirname(fileURLToPath(import.meta.url)))
// The repository's own docs/, not the generated copy under site/. Those are the
// files GitHub renders and `generate-docs-llms-full.sh` bundles, so they are
// where the committed SVG has to live. The generated copy gets the island.
const DOCS = join(dirname(SITE), 'docs')
const OUT_DIR = join(DOCS, 'diagrams')
const OUT_REL = './diagrams'

const CHECK = process.argv.includes('--check')

// The generated block is identified by the paths inside it rather than by a
// marker comment. MDX does not permit HTML comments at all -- `<!-- … -->` is a
// syntax error there, which is how the first attempt broke the site.
//
// It is a <picture>, not a bare <img>, because github.com has a dark theme and
// these diagrams are opaque: a light-mode render is a white slab against a dark
// page. GitHub honours <picture> with a prefers-color-scheme <source>, so both
// renders are committed and the reader gets the one matching their theme.
// Everything here is valid in MDX as well as GitHub-flavoured markdown.
const GENERATED = new RegExp(`\\n?<picture>[\\s\\S]*?${OUT_REL}/[^"]+[\\s\\S]*?</picture>`, 'g')
// Older single-<img> blocks, so a file written before this change is cleaned up
// rather than accumulating a second block beside it.
const LEGACY_IMG = new RegExp(`\\n?<img src="${OUT_REL}/[^"]+"[^>]*/>`, 'g')

/** ```c4 [meta]\n …source… \n``` */
const FENCE = /^```c4([^\n]*)\n([\s\S]*?)\n```$/gm

// Counted separately because they are different things: one diagram produces
// two SVGs (light and dark) and edits one page. Reporting a single total made
// the output say "4 diagram(s)" for two of them.
let diagrams = 0
let svgsWritten = 0
let pagesWritten = 0
let stale = []

// Top level only: adr/ holds 84 immutable decision records that are not part of
// the site and must not have generated markup written into them, and history/
// and spikes/ are internal engineering write-ups.
const pages = readdirSync(DOCS, { withFileTypes: true })
  .filter((e) => e.isFile() && (e.name.endsWith('.mdx') || e.name.endsWith('.md')))
  .map((e) => e.name)
const expected = new Set()

for (const page of pages) {
  const path = join(DOCS, page)
  const original = readFileSync(path, 'utf8')

  // Start from a copy with any previously generated blocks removed, so the
  // markers never accumulate and the diff is stable across runs.
  const cleaned = original.replace(GENERATED, '').replace(LEGACY_IMG, '')

  let index = 0
  let out = ''
  let last = 0

  for (const match of cleaned.matchAll(FENCE)) {
    index += 1
    const source = match[2]
    const meta = match[1] || ''

    // Some fences are invalid on purpose — a page documenting parse errors needs
    // one. Marked with `invalid`, they are skipped rather than failing the run,
    // and get no image, since there is nothing to render.
    if (/\binvalid\b/.test(meta)) continue
    const stem = `${basename(page).replace(/\.mdx?$/, '')}-${index}`
    diagrams += 1
    const light = `${stem}.svg`
    const dark = `${stem}-dark.svg`
    expected.add(light)
    expected.add(dark)

    let failed = false
    for (const [name, isDark] of [
      [light, false],
      [dark, true],
    ]) {
      const { svg, errors } = renderC4DiagramToSVG(source, isDark)
      if (errors.length) {
        console.error(`FAILED  ${page} diagram ${index}: ${errors.map((e) => e.message).join('; ')}`)
        process.exitCode = 1
        failed = true
        break
      }

      const target = join(OUT_DIR, name)
      const current = existsSync(target) ? readFileSync(target, 'utf8') : null
      if (current !== svg) {
        if (CHECK) stale.push(`${OUT_REL}/${name}`)
        else {
          mkdirSync(OUT_DIR, { recursive: true })
          writeFileSync(target, svg)
          svgsWritten += 1
        }
      }
    }
    if (failed) continue

    // The picture goes immediately after the fence. GitHub shows both: the
    // source and the rendering. Alt text names the page rather than describing
    // the diagram, because a generated description would be worse than none.
    const end = match.index + match[0].length
    out += cleaned.slice(last, end)
    out +=
      `\n<picture>` +
      `<source media="(prefers-color-scheme: dark)" srcset="${OUT_REL}/${dark}" />` +
      `<img src="${OUT_REL}/${light}" alt="Diagram ${index} from ${page}" />` +
      `</picture>`
    last = end
  }
  out += cleaned.slice(last)

  if (out !== original) {
    if (CHECK) stale.push(page)
    else {
      writeFileSync(path, out)
      pagesWritten += 1
    }
  }
}

// Remove SVGs whose fence is gone, so deleting a diagram does not leave an
// orphan committed forever.
if (existsSync(OUT_DIR)) {
  for (const f of readdirSync(OUT_DIR)) {
    if (f.endsWith('.svg') && !expected.has(f)) {
      if (CHECK) stale.push(`${OUT_REL}/${f} (orphaned)`)
      else {
        rmSync(join(OUT_DIR, f))
        svgsWritten += 1
      }
    }
  }
}

if (CHECK) {
  if (stale.length) {
    console.error('Diagrams are out of date. Run `pnpm diagrams` and commit the result:')
    for (const s of stale) console.error(`  ${s}`)
    process.exit(1)
  }
  console.log(`All ${diagrams} diagram(s) up to date (${expected.size} SVGs).`)
} else {
  console.log(
    `${diagrams} diagram(s): ${svgsWritten} SVG(s), ${pagesWritten} page(s) written.`,
  )
}
