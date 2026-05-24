# Open-source launch architecture for liminis-graph

**Status:** Discussion input, not a decision. Captured 2026-05-23 during planning conversation about open-sourcing liminis-graph.

This document captures the architectural questions, options, and trade-offs that surfaced during a discussion about preparing liminis-graph for OSS release. It's input to a future debate — no commitments here.

## Context

liminis-graph today is the Rust knowledge graph engine used by liminis-app (Electron) via Unix-socket JSON-RPC. liminis-app provides the MCP layer in-process (TS providers calling `GraphitiSocketClient`) and bundles a Python embedder sidecar. The Rust binary itself only speaks one transport (socket) and assumes its peers handle MCP, embedding, LLM, etc.

For OSS distribution, the typical user is a developer who wants to drop a knowledge-graph backend into their AI agent workflow — most plausibly via Claude Desktop / Claude Code / Cline / other MCP-aware clients. They want a single binary they download, point at, and use. They do not have liminis-app, do not want a Python sidecar, do not want a multi-process setup.

## Question 1 — where does the MCP server live?

### Options considered

**A. MCP embedded in the Rust binary, as a second transport mode**

```
liminis-graph --mcp-stdio --db ~/.graphiti/db    # for MCP clients
liminis-graph --socket /path/to.sock --db ...    # for embedded apps
```

The handler layer (`handlers::dispatch`) is transport-agnostic today. MCP stdio becomes a new transport that maps each `knowledge_*` method to an MCP `tool()`. Uses `rmcp` (official Rust MCP SDK). Single binary, two modes, dispatched by CLI flag.

- **Pros:** lowest possible OSS friction (download → add to mcp.json → done); single artifact to release; no inter-process plumbing; one repo to maintain; supports both solo MCP and embedded socket use cases without compromise.
- **Cons:** Rust binary takes on MCP protocol awareness (release cadence couples to MCP spec evolution); `rmcp` becomes a runtime dependency; the binary has two modes to test.

**B. Out-of-process companion MCP wrapper (separate process)**

```
Claude Desktop → stdio → python-mcp-wrapper → Unix socket → liminis-graph → DB
```

A small Python or TS shim speaks MCP on one end and JSON-RPC over socket to liminis-graph on the other. This is the canonical MCP pattern (most published MCP servers are language-specific shims wrapping a backend service).

- **Pros:** clean separation of concerns (Rust binary stays pure DB engine); MCP protocol changes don't force Rust releases; easy to add wrappers in multiple languages; multiple concurrent MCP clients fit naturally (each connects to one wrapper, all wrappers share the one socket).
- **Cons:** two processes for users to install + supervise; more setup friction; OSS user typically wants "download one binary" UX and doesn't get it; if the wrapper is Python, adds Python dep to the OSS install story.

**C. MCP-in-binary with optional native socket bypass**

Like (A), but framed as "binary IS an MCP server, socket mode is the advanced opt-in." Same code shape, different framing.

### Initial lean

Option **A**. Both transports in one binary, handler dispatch shared. The OSS-friendly mode (stdio MCP) becomes the default the README recommends; socket mode stays for embedded apps like liminis-app.

Rationale: the canonical MCP-wrapper pattern (Option B) exists because backends are *external services* and the wrapper marshals across a boundary. liminis-graph IS the backend — there's no marshaling boundary to bridge. A separate wrapper would just add a hop without separating any real concern.

Counter-arguments parked but worth revisiting at decision time:

| Concern | Counter |
|---|---|
| Rust binary takes on MCP awareness | Handler dispatch is the real contract; MCP is just plumbing |
| MCP spec evolves | True for any MCP impl; `rmcp` tracks the spec |
| Multiple concurrent clients | Socket mode handles it; OSS user typically has one stdio client |
| Bigger binary | `rmcp` is small; stdio is std-only |

## Question 2 — what about the embedder?

This is a larger obstacle than the MCP question. The Rust binary currently expects an HTTP embedder at `127.0.0.1:8765`. In liminis-app this is satisfied by a Python sidecar that loads BGE-base via `sentence-transformers`. For OSS, users need *some* solution.

### Options

**a. Document the Python sidecar, ship setup instructions**
Friction-acceptable for hacker audience. Requires Python + uv + ~1GB of torch + model download on first use.

**b. Bundle a single-file PyInstaller binary of the sidecar**
Larger download, no Python install requirement, still ships PyTorch internals. Cross-platform build matrix needed.

**c. Build native embedding into the Rust binary (e.g., via `candle` or ONNX Runtime)**
Eliminates the second process entirely. But this violates **Principle V** ("no ML runtime in the Rust crate"), which was set during liminis-graph's design to keep the binary lean and offload ML to Apple Silicon / out-of-process services.

For OSS, Principle V may not be the right rule. The principle was scoped to liminis's specific Apple-Silicon + ANE strategy. An OSS user on Linux has no ANE option — out-of-process embedding is pure cost with no architectural payoff. Worth revisiting at OSS-launch time.

**d. CoreML embedder (spike #787, GO decision)**
Mac-only. Fast and lightweight. Doesn't help Linux/Windows OSS users.

**e. Make embedder pluggable via HTTP, document several deployment patterns**
- Python sentence-transformers sidecar (cross-platform, current default)
- CoreML Swift sidecar (Mac, fast)
- Cloud embedding API (OpenAI, Voyage, Cohere) — just point `GRAPHITI_EMBEDDING_URL` at the right endpoint
- Native embedder mode (if Principle V is relaxed)

Recommend `e` as the framing. The Rust binary stays embedder-agnostic; users pick the deployment that fits their setup.

## Question 3 — LLM extractor portability

Today `AnthropicExtractor` is hardcoded to Anthropic's API shape. For OSS:

- Generalize to `OpenAICompatibleExtractor` — works against Anthropic, OpenAI, Ollama, LM Studio, OpenRouter, vLLM, etc.
- Env vars: `LLM_API_URL`, `LLM_API_KEY`, `LLM_MODEL`
- Document the "fully local" path: Ollama + Qwen3.6-27b (per `project_extraction_eval_results.md`, ~7pp F1 gap to Sonnet, free)
- Document the "cloud" path: Anthropic Sonnet, the proven path

Small change; high enablement value for OSS. No major design questions.

## Question 4 — LadybugDB licensing

Need to confirm LadybugDB itself is OSS-compatible before announcing. The community fork should be (it's a KuzuDB fork after KuzuDB went EOL), but check the actual license file. If there's any commercial restriction, that's a blocker for OSS launch.

## Question 5 — LLM prompt portability

The `add_episode` extraction prompts may be implicitly tuned for Sonnet. The April 2026 extraction-quality eval showed local Qwen models work but with measurable F1 gap. For OSS users on Ollama/local-LLM setups, prompt-portability matters.

Not urgent. Re-run eval if/when an OSS user reports extraction quality issues.

## Question 6 — Release ergonomics

Standard OSS chore-work, not architecturally interesting:

- Prebuilt binaries via GitHub Actions release workflow (Mac arm64, Mac x86_64, Linux x86_64, possibly Linux arm64)
- README with quick-start: download → add to mcp.json → use
- LICENSE file (MIT or Apache 2.0?)
- CONTRIBUTING guide
- Issue templates
- Embedder setup docs (link to recommended deployments per Question 2)
- LLM setup docs (link to Ollama recipe for fully-local path)

## Suggested issue chain (if/when this gets prioritized)

In dependency order:

1. **MCP-over-stdio transport in Rust binary** — `rmcp` dep, `--mcp-stdio` CLI flag, tool wiring for all 24 handlers, stdio main loop. Independent of OSS push; useful even for internal experimentation. Acceptance: Claude Desktop connects directly and calls every method.

2. **Configurable LLM extractor for OpenAI-compatible endpoints** — generalize `AnthropicExtractor`. Independent.

3. **Embedder strategy decision** — either re-examine Principle V (would gate a native embedder issue) or commit to multi-deployment pluggability + ship docs. Decision needed before OSS launch.

4. **LadybugDB license verification** — quick due-diligence pass. Blocks OSS launch if it surfaces anything.

5. **OSS launch checklist** — README, LICENSE, release workflow, install docs. Blocked by 1, 2, 3, 4.

## What we explicitly did not decide

- Whether to open-source. The "if" is your call.
- Timing.
- Repository org (stays in `verveguy/liminis-graph` or moves to a new home?).
- Whether liminis-app's OSS posture changes (it's currently public per `gh repo list`).
- Naming / rebranding (`liminis-graph` is fine but worth considering whether a more discoverable name helps).

## References

- Audit: `~/.claude/projects/-Users-bpja-dev-liminis-project/memory/project_graphiti_integration_audit_2026_05_21.md`
- ANE strategy doc: `liminis/docs/project_notes/designs/apple-neural-engine-opportunities.md`
- CoreML spike: `liminis#787` (merged with GO decision)
- Extraction-quality eval: `~/.claude/projects/-Users-bpja-dev-liminis-project/memory/project_extraction_eval_results.md`
- Tier 1a–1c specs in `specs/`: define the 24-method API surface that the MCP wrapping would expose
