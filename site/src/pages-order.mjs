/**
 * Reading order for the documentation pages, and the sidebar labels.
 *
 * Shared by scripts/sync-docs.mjs (which writes it into each generated page's
 * frontmatter) and astro.config.mjs (which builds the sidebar from it), so the
 * two cannot disagree about what comes first.
 *
 * The order matches the one `scripts/generate-docs-llms-full.sh` bundles these
 * pages in. That is the same argument about what to read first, and it would be
 * odd for the site and the LLM bundle to answer it differently.
 *
 * Pages are served at the top level — /liminis-context-graph/ontology/, not
 * under a /guide/ prefix. Those URLs are what the README, the llms bundles and
 * anything else linking here already use, and the Jekyll site served them.
 */
export const PAGES = [
  ['getting-started', 'Getting Started'],
  ['configuration', 'Configuration'],
  ['ipc-mcp-reference', 'IPC & MCP Reference'],
  ['telemetry', 'Telemetry'],
  ['ontology', 'Ontology'],
  ['operations', 'Operations'],
  ['testing-and-evaluation', 'Testing & Evaluation'],
  ['eval-full-corpus-runbook', 'Full-Corpus Benchmark Runbook'],
  ['extraction-quality-evaluation', 'Extraction-Quality Evaluation'],
  ['release-process', 'Release Process'],
]

export const ORDER = PAGES.map(([slug]) => slug)
