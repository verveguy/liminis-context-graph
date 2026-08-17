//! Shared seeding harness (issue #234) for MCP-over-stdio suites that need a workspace backed
//! by the real-corpus WAL fixture (`crates/core/tests/fixtures/real_corpus_wal/`, #217) instead
//! of an empty graph. Built for `mcp_real_corpus_e2e.rs`'s read-path suite, but written and
//! documented generically so the follow-on write-path and admin-path MCP suites can call
//! [`seed_real_corpus_workspace`] unmodified (User Story 2, FR-001/FR-002).
//!
//! **Zero LLM/embedder calls, by construction**: WAL replay (`knowledge_rebuild_from_wal`) is
//! 100% mechanical Cypher re-execution — the fixture's stored embeddings were captured at
//! ingest time by a real embedder and are baked into the WAL itself. The only network surface
//! touched during seeding is the `--embedder-http` stub URL the caller supplies (e.g.
//! `spawn_stub_embedder()`), used solely for query-time embedding should the seeding process
//! need it (it doesn't, today, but the flag is required to start the binary at all).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

use super::{binary_path, McpClient};

/// The committed real-corpus WAL fixture, resolved relative to this crate's manifest dir
/// (`crates/service`) rather than `crates/core`'s, since `CARGO_MANIFEST_DIR` here is
/// `crates/service`.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/real_corpus_wal")
}

/// A temp workspace whose `.lcg`-equivalent `db_path`/`wal_dir` have been seeded from the
/// real-corpus WAL fixture and rebuilt (zero LLM/embedder calls), ready to back one or more
/// `--mcp-stdio` subprocesses. Reopening `db_path` from a fresh process is cheap and idempotent
/// (see `spawn_reader`'s doc comment), so a single `SeededWorkspace` can safely back several
/// separately-scoped subprocesses across a test file without re-paying the rebuild cost.
pub struct SeededWorkspace {
    /// Backing temp directory, kept alive for the workspace's lifetime — dropping this value
    /// deletes `db_path`/`wal_dir` along with it.
    pub dir: TempDir,
    pub db_path: PathBuf,
    pub wal_dir: PathBuf,
    /// The fixture's `expected_results.json`, parsed — golden counts, queries, traversal, and
    /// relation-type samples recorded at capture time (#217).
    pub expected: Value,
}

impl SeededWorkspace {
    /// The fixture's single group ID (`expected_results.json`'s `group_id`, `"apollo_program"`
    /// as of #217's capture).
    pub fn group_id(&self) -> &str {
        self.expected["group_id"]
            .as_str()
            .expect("expected_results.json must have a string group_id")
    }

    pub fn embedding_dim(&self) -> usize {
        self.expected["embedding_dim"]
            .as_u64()
            .expect("expected_results.json must have an embedding_dim") as usize
    }

    /// Spawns a fresh `--mcp-stdio` subprocess reopening this workspace's already-rebuilt
    /// `db_path`. Reopening is cheap and idempotent — `main.rs`'s eager startup index build
    /// (`build_indices_and_constraints`) swallows "already exists" for indices that survived
    /// the seeding process's own clean shutdown — so this does not re-run the WAL replay or
    /// re-pay its ~60-140s cost. `extra_args` typically carries `--scope=...`.
    pub fn spawn_reader(&self, embedder_url: &str, extra_args: &[&str]) -> McpClient {
        let mut cmd = Command::new(binary_path());
        cmd.env("LCG_DB_PATH", self.db_path.to_str().unwrap())
            .env("LCG_WAL_DIR", self.wal_dir.to_str().unwrap())
            // No ANTHROPIC_API_KEY/sidecar/LCG_EXTRACTION_URL in the CI test environment, and no
            // --extractor-uds/--extractor-http flag either: since #331, a missing extraction
            // provider no longer prevents startup — this suite never calls extraction, so there
            // is nothing left to work around (previously required the never-dialed placeholder
            // `--extractor-http http://127.0.0.1:1/v1/chat/completions`, per #330).
            .args(["--mcp-stdio", "--embedder-http", embedder_url])
            .args(extra_args);
        McpClient::spawn(cmd)
    }

    /// Recursively copies this workspace's already-rebuilt `db_path`/`wal_dir` into a fresh
    /// `TempDir`, returning a new, independently-mutable `SeededWorkspace` (issue #235,
    /// FR-002). `Db::open` embeds no path-derived state and nothing holds an OS lock past a
    /// clean process exit, so copying the on-disk files while no process has them open and
    /// reopening the copy from a fresh subprocess is safe — this is strictly cheaper than
    /// re-seeding (~60-140s) since it skips WAL replay and index rebuild entirely.
    ///
    /// Callers must not hold any `McpClient` open against `self` (or a prior fork) while
    /// forking — the source files must be quiescent for the copy to be consistent.
    pub fn fork(&self) -> SeededWorkspace {
        let dir = TempDir::new().expect("create fork temp dir");
        let db_path = dir.path().join("real_corpus.db");
        let wal_dir = dir.path().join("wal");

        copy_dir_recursive(&self.db_path, &db_path);
        copy_dir_recursive(&self.wal_dir, &wal_dir);
        // If lbug represented db_path as a single file (rather than a directory), it may have
        // written sibling files next to it (e.g. `<db>.wal`, `<db>.lock`) — mirrors the sibling
        // cleanup handle_clear_all performs in the file-DB case. Copy them too, if present.
        for ext in &[".wal", ".lock"] {
            let sibling = PathBuf::from(format!("{}{}", self.db_path.display(), ext));
            if sibling.exists() {
                let dest = PathBuf::from(format!("{}{}", db_path.display(), ext));
                std::fs::copy(&sibling, &dest).unwrap_or_else(|e| {
                    panic!(
                        "copy sibling {} to {}: {e}",
                        sibling.display(),
                        dest.display()
                    )
                });
            }
        }

        SeededWorkspace {
            dir,
            db_path,
            wal_dir,
            expected: self.expected.clone(),
        }
    }

    /// Spawns a fresh `--mcp-stdio` subprocess reopening this workspace's already-rebuilt
    /// `db_path`, with `LIMINIS_WORKSPACE_ROOT` set to `workspace_root` and `--extractor-http`
    /// pointed at a real, listening stub extraction endpoint (issue #235) — unlike
    /// `spawn_reader`'s never-dialed URL, mutation tools that classify/extract
    /// (`knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_reprocess_entity_types`,
    /// `knowledge_reprocess_relation_types`) actually call this endpoint. `workspace_root` must
    /// already contain any `.lcg/ontology.yaml` / `.liminis/knowledge-corrections.yaml` the
    /// caller's assertions depend on — both are read once at process startup, not per-request.
    pub fn spawn_mutator(
        &self,
        embedder_url: &str,
        extractor_url: &str,
        workspace_root: &Path,
        extra_args: &[&str],
    ) -> McpClient {
        let mut cmd = Command::new(binary_path());
        cmd.env("LCG_DB_PATH", self.db_path.to_str().unwrap())
            .env("LCG_WAL_DIR", self.wal_dir.to_str().unwrap())
            .env("LIMINIS_WORKSPACE_ROOT", workspace_root.to_str().unwrap())
            .args(["--mcp-stdio", "--embedder-http", embedder_url])
            .args(["--extractor-http", extractor_url])
            .args(extra_args);
        McpClient::spawn(cmd)
    }
}

/// Recursively copies `src` to `dst` (`dst`'s parent must already exist; `dst` itself must not).
/// Used by [`SeededWorkspace::fork`] to clone an already-rebuilt workspace's on-disk files.
fn copy_dir_recursive(src: &Path, dst: &Path) {
    if src.is_dir() {
        std::fs::create_dir_all(dst)
            .unwrap_or_else(|e| panic!("create dir {}: {e}", dst.display()));
        for entry in
            std::fs::read_dir(src).unwrap_or_else(|e| panic!("read dir {}: {e}", src.display()))
        {
            let entry = entry.expect("read dir entry");
            copy_dir_recursive(&entry.path(), &dst.join(entry.file_name()));
        }
    } else {
        std::fs::copy(src, dst)
            .unwrap_or_else(|e| panic!("copy {} to {}: {e}", src.display(), dst.display()));
    }
}

/// Seeds a fresh temp workspace from the committed real-corpus WAL fixture and rebuilds it
/// through a real `--mcp-stdio` subprocess (spawned with `--scope=admin`), then shuts that
/// process down cleanly before returning — so the caller can freely spawn additional processes
/// (e.g. `--scope=read`) against the resulting `db_path` (FR-001, FR-002, FR-003, FR-013).
///
/// The fixture's `wal/*.jsonl` files are copied into a fresh location under a `TempDir` rather
/// than pointed at directly — the checked-in fixture directory must never be written into,
/// including by a future write-path suite reusing this same helper.
///
/// `knowledge_rebuild_from_wal` called without a progress token returns a background job id
/// immediately rather than blocking (`handlers::handle_rebuild_from_wal` always spawns a
/// background job when `progress_tx` is `None`, regardless of `dry_run`) — this function uses
/// `call_tool_with_progress` to drive the rebuild synchronously to completion instead, per the
/// precedent in `mcp_progress.rs`.
///
/// `embedder_url` should point at a loopback stub embedder (e.g.
/// `spawn_stub_embedder()`'s URL) — the seeding process never calls a real embedder over the
/// network; WAL replay is mechanical Cypher re-execution using the fixture's already-embedded
/// content.
///
/// Panics on any failure: this is test setup, not a code path under test.
pub fn seed_real_corpus_workspace(embedder_url: &str) -> SeededWorkspace {
    let expected: Value = {
        let path = fixture_dir().join("expected_results.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{} must be valid JSON: {e}", path.display()))
    };

    let dir = TempDir::new().expect("create temp workspace dir");
    let db_path = dir.path().join("real_corpus.db");
    let wal_dir = dir.path().join("wal");

    std::fs::create_dir_all(&wal_dir).expect("create wal dir");
    let src_wal_dir = fixture_dir().join("wal");
    for entry in std::fs::read_dir(&src_wal_dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", src_wal_dir.display()))
    {
        let entry = entry.expect("read fixture wal dir entry");
        let dest = wal_dir.join(entry.file_name());
        std::fs::copy(entry.path(), &dest).unwrap_or_else(|e| {
            panic!(
                "failed to copy {} to {}: {e}",
                entry.path().display(),
                dest.display()
            )
        });
    }
    // The committed fixture ships no `.wal-generation.json` (it predates issue #387, and is
    // itself an example of the dot-namespace-dropping publish step issue #414 documents) — this
    // flat copy is exactly the genuine pre-378 legacy layout issue #428's migration-time fix
    // targets. The seeder subprocess's startup migration (`migrate_wal_root_if_needed`) both
    // relocates the `*.jsonl` files into `<wal_root>/liminis/` *and* stamps a generation for the
    // resulting stream (issue #428 FR-001) — no separate pre-stamping step is needed here
    // anymore. See the assertion after the seeding rebuild below, which turns this into a live
    // regression check for every downstream suite built on this harness.

    let mut seeder = McpClient::spawn({
        let mut cmd = Command::new(binary_path());
        cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
            .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
            .env("LCG_SHUTDOWN_TIMEOUT_MS", "5000")
            .args([
                "--mcp-stdio",
                "--embedder-http",
                embedder_url,
                "--scope=admin",
                // Never dialed — WAL replay makes zero extraction calls; the binary just
                // refuses to start at all without some extractor configured (see
                // spawn_reader's doc comment above).
                "--extractor-http",
                "http://127.0.0.1:1/v1/chat/completions",
            ]);
        cmd
    });
    seeder.initialize();

    // A full replay + HNSW/FTS index build over this fixture's 1,506 entities takes roughly
    // 60-140s (per real_corpus_wal/README.md) — generous timeout to avoid CI flakiness.
    let rebuild = seeder.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({}),
        "seed-real-corpus",
        Duration::from_secs(600),
    );
    assert!(
        rebuild["result"]["isError"].as_bool() != Some(true),
        "knowledge_rebuild_from_wal failed while seeding the real-corpus workspace: {rebuild:?}"
    );
    let rebuild_content = &rebuild["result"]["structuredContent"];
    assert_eq!(
        rebuild_content["success"],
        json!(true),
        "seeding rebuild did not report success: {rebuild:?}"
    );
    assert_eq!(
        rebuild_content["indices_built"],
        json!(true),
        "seeding rebuild did not build indices: {rebuild:?}"
    );

    // Issue #428 regression check: the seeder subprocess's startup migration must have stamped
    // a generation for the default group as part of relocating the flat fixture copy (FR-001) —
    // turning every downstream suite built on this harness into a live check that migration
    // keeps producing a replayable, non-refused workspace.
    let default_group_dir =
        lcg_core::wal_group::group_wal_dir(&wal_dir, lcg_core::wal_group::DEFAULT_GROUP_ID)
            .expect("encode default group WAL dir name");
    assert!(
        lcg_core::wal_generation::read_generation(&default_group_dir).is_some(),
        "the seeder subprocess's startup migration must stamp a readable .wal-generation.json \
         for the default group (issue #428 FR-001)"
    );

    // Clean shutdown (never kill()) so the just-completed rebuild's WAL checkpoint is durable
    // before any other process reopens db_path (ADR-0017) — a killed seeding process risks the
    // reader process hitting degraded-mode recovery on a workspace that was never actually
    // broken, silently defeating the "seed once, reuse" design (FR-013).
    let close = seeder.call_tool("knowledge_close", json!({}));
    assert!(
        close["result"]["isError"].as_bool() != Some(true),
        "knowledge_close failed while shutting down the seeding process: {close:?}"
    );
    let status = seeder.wait_for_exit(Duration::from_secs(15));
    assert_eq!(
        status.code(),
        Some(0),
        "seeding process did not exit cleanly after knowledge_close"
    );

    SeededWorkspace {
        dir,
        db_path,
        wal_dir,
        expected,
    }
}
