// Binary-level migration integration test (SC-001, SC-002, FR-002).
//
// Spawns the real binary against a legacy .graphiti/ workspace, waits for the socket to bind,
// then confirms migration has actually completed via a blocking IPC round-trip (socket-bound
// alone is not sufficient — see the comment at that call site), then asserts:
// 1. The service is not degraded and responds to IPC
// 2. Entity counts match the pre-migration state (0, since the legacy DB was empty)
// 3. The correct post-migration file layout under .lcg/

#[cfg(unix)]
mod migration_binary_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use lcg_core::db::Db;
    use tempfile::TempDir;

    fn wait_for_socket(socket_path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if socket_path.exists() && UnixStream::connect(socket_path).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn send_sigterm(pid: u32) {
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("kill command failed");
    }

    /// Builds a legacy `.graphiti/` workspace fixture at `workspace` (DB, 5 WAL `*.jsonl` files,
    /// a `.wal-generation.json` sidecar with a known id, ontology + hash sidecars) — the same
    /// corpus `binary_migrates_legacy_workspace_on_startup` uses, factored out so the #442
    /// custom-`LCG_WAL_DIR` regression test below can build an identical fixture without
    /// duplicating ~40 lines of setup. Returns the WAL file names and the legacy generation id.
    fn build_legacy_workspace_fixture(workspace: &Path) -> (&'static [&'static str], &'static str) {
        let legacy = workspace.join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();

        let legacy_db_path = legacy.join("db");
        let db = Db::open(legacy_db_path.to_str().unwrap()).unwrap();
        drop(db);

        let wal_dir = legacy.join("wal");
        std::fs::create_dir(&wal_dir).unwrap();
        const WAL_FILE_NAMES: &[&str] = &[
            "001.jsonl",
            "002.jsonl",
            "003.jsonl",
            "004.jsonl",
            "005.jsonl",
        ];
        for name in WAL_FILE_NAMES {
            std::fs::write(wal_dir.join(name), b"{\"type\":\"test\"}\n").unwrap();
        }

        const LEGACY_GENERATION_ID: &str = "11111111-1111-1111-1111-111111111111";
        std::fs::write(
            wal_dir.join(".wal-generation.json"),
            format!("{{\"generation\":\"{LEGACY_GENERATION_ID}\"}}"),
        )
        .unwrap();

        std::fs::write(
            legacy.join("ontology.yaml"),
            b"entities:\n  - name: Person\n",
        )
        .unwrap();
        std::fs::write(legacy.join("ontology-hash.json"), b"{\"hash\":\"abc123\"}").unwrap();

        (WAL_FILE_NAMES, LEGACY_GENERATION_ID)
    }

    fn spawn_stub_embedder() -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            let embedding = format!("[{}]", vec!["0.0"; 768].join(","));
            let body = format!(
                r#"{{"object":"list","data":[{{"object":"embedding","embedding":{embedding},"index":0}}],"model":"stub-model","usage":{{"prompt_tokens":1,"total_tokens":1}}}}"#
            );
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            for stream in listener.incoming() {
                let Ok(mut s) = stream else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = Read::read(&mut s, &mut buf);
                let _ = Write::write_all(&mut s, http_response.as_bytes());
            }
        });

        port
    }

    // FR-001, FR-002, FR-003, FR-006, FR-007, SC-001, SC-002:
    // Binary migrates a legacy .graphiti/ workspace to .lcg/ on startup and then
    // serves IPC requests normally without data loss.
    //
    // Regression guard for #437: `migration::migrate_workspace` (the .graphiti/->.lcg/ move)
    // must complete before `bootstrap_app_state`'s per-group WAL-root migration
    // (`wal_group::migrate_wal_root_if_needed`) inspects `.lcg/wal` for loose files to
    // relocate — otherwise, for a .graphiti-era workspace, `.lcg/wal` doesn't exist yet at
    // that point, the per-group migration no-ops on the missing directory, and the legacy
    // WAL content that migrate_workspace moves in afterward is left loose at .lcg/wal/'s
    // top level forever (invisible to this process — see the comments at both call sites in
    // main.rs). Uses a 5-file corpus, not a single file, to distinguish "one file happens to
    // relocate correctly" from "every file in the set is relocated," and includes a pre-existing
    // .wal-generation.json sidecar (issue #431) with a known id in the legacy fixture so the
    // assertions can confirm it is *relocated* (its content survives) under the group directory
    // rather than loose at the root or silently replaced by a fresh mint.
    #[test]
    fn binary_migrates_legacy_workspace_on_startup() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path();

        // ── Build legacy .graphiti/ workspace fixture ────────────────────────
        let legacy = workspace.join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();

        // Create a raw lbug DB without init_schema. We intentionally skip init_schema
        // here: lbug stores vector-extension catalog state that triggers an internal
        // assertion when LOAD EXTENSION runs again after a file rename. The binary's
        // own init_schema (called after migration) is the first time schema is applied
        // to this DB, which is the normal production path.
        let legacy_db_path = legacy.join("db");
        let db = Db::open(legacy_db_path.to_str().unwrap()).unwrap();
        drop(db);

        // Application WAL directory with several dummy JSONL files (FR-007: a more realistic
        // multi-file corpus than a single file, to catch a fix that only relocates the first
        // entry it sees rather than every loose file at the root).
        let wal_dir = legacy.join("wal");
        std::fs::create_dir(&wal_dir).unwrap();
        let wal_file_names = [
            "001.jsonl",
            "002.jsonl",
            "003.jsonl",
            "004.jsonl",
            "005.jsonl",
        ];
        for name in wal_file_names {
            std::fs::write(wal_dir.join(name), b"{\"type\":\"test\"}\n").unwrap();
        }

        // FR-007/Acceptance Scenario 2: a pre-existing `.wal-generation.json` sidecar in the
        // legacy fixture, with a known generation id, so the post-migration assertions below can
        // distinguish "migration relocated the original sidecar" (this id survives, byte-for-byte)
        // from "migration only stamped a fresh one" (a different id would appear) — the two are
        // indistinguishable by an existence-only check. `wal_group::migrate_wal_root_if_needed`
        // relocates any loose `.wal-generation.json` at the WAL root ahead of minting a new one
        // (see its doc comment), so a genuine legacy sidecar, once it lands loose at `.lcg/wal/`
        // via the whole-directory `.graphiti/wal` -> `.lcg/wal` rename, must win over a fresh mint.
        let legacy_generation_id = "11111111-1111-1111-1111-111111111111";
        std::fs::write(
            wal_dir.join(".wal-generation.json"),
            format!("{{\"generation\":\"{legacy_generation_id}\"}}"),
        )
        .unwrap();

        // Ontology and hash sidecar
        std::fs::write(
            legacy.join("ontology.yaml"),
            b"entities:\n  - name: Person\n",
        )
        .unwrap();
        std::fs::write(legacy.join("ontology-hash.json"), b"{\"hash\":\"abc123\"}").unwrap();

        // ── Spawn binary ─────────────────────────────────────────────────────
        // Socket path is absolute so we can wait for it. DB path uses binary default
        // (.lcg/db/liminis.db relative to cwd = workspace) — no LCG_DB_PATH set.
        // The binary will run migration, move files, then start normally.
        let socket_path = workspace.join(".lcg").join("service.sock");
        let embedder_port = spawn_stub_embedder();
        let embedder_url = format!("http://127.0.0.1:{embedder_port}/v1/embeddings");

        let binary = env!("CARGO_BIN_EXE_liminis-context-graph");
        let mut child = Command::new(binary)
            .current_dir(workspace)
            .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
            .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
            .args(["--embedder-http", &embedder_url])
            // With no ANTHROPIC_API_KEY, no sidecar socket, and no LCG_EXTRACTION_URL in the
            // CI environment, startup would otherwise fail fast per FR-011. This test never
            // exercises extraction, so the URL need not be reachable.
            .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
            .spawn()
            .expect("failed to spawn liminis-context-graph");

        let ready = wait_for_socket(&socket_path, Duration::from_secs(30));
        if !ready {
            child.kill().ok();
            panic!("service did not become ready within 30s — migration may have failed");
        }

        // ── Assert IPC is functional post-migration ───────────────────────────
        // Deliberately performed *before* the filesystem assertions below. `wait_for_socket`
        // only proves the socket is bound — main.rs binds it, and prints "listening on ...",
        // before calling `bootstrap_app_state` (ADR-0009: this lets health_check/recovery IPC
        // work even in a degraded state). `bootstrap_app_state` is what runs the per-group WAL
        // migration this test exists to guard (#437), so a socket-bound check alone races
        // against it: the OS accepts connect() into the listen backlog as soon as bind()
        // returns, whether or not the server has called accept() yet, let alone finished
        // migrating. A blocking `knowledge_status` round-trip only returns once
        // `run_socket_service` is actually processing requests, which happens after
        // `bootstrap_app_state().await` resolves — so a successful response here is the
        // earliest point at which the filesystem assertions below are safe to make.
        let mut stream =
            UnixStream::connect(&socket_path).expect("failed to connect to service socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();

        let req = r#"{"jsonrpc":"2.0","id":1,"method":"knowledge_status","params":{}}"#;
        writeln!(stream, "{req}").expect("failed to write knowledge_status request");

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("failed to read knowledge_status response");

        assert!(
            !response.is_empty(),
            "expected a response from knowledge_status"
        );

        let resp: serde_json::Value =
            serde_json::from_str(response.trim()).expect("knowledge_status response is not JSON");

        assert!(
            resp.get("result").is_some(),
            "expected result field in knowledge_status response: {resp}"
        );
        // In the healthy (non-degraded) code path, the response has context_graph_initialized=true
        // rather than a `degraded` field. Assert the healthy path was taken.
        assert_eq!(
            resp["result"]["context_graph_initialized"],
            serde_json::Value::Bool(true),
            "service must not be in degraded mode after successful migration: {resp}"
        );
        // SC-002 count parity: pre-migration count was 0 (no entities added),
        // post-migration count must still be 0.
        assert_eq!(
            resp["result"]["entity_count"],
            serde_json::Value::Number(0.into()),
            "entity count must match pre-migration count (0): {resp}"
        );

        drop(reader);

        // ── Assert post-migration file layout (FR-002) ────────────────────────
        let new_dir = workspace.join(".lcg");
        assert!(
            !legacy.exists(),
            ".graphiti/ must be removed after migration"
        );
        assert!(
            new_dir.join("db").is_dir(),
            ".lcg/db/ must be a directory, not a file"
        );
        assert!(
            new_dir.join("db").join("liminis.db").is_file(),
            ".lcg/db/liminis.db must exist"
        );
        assert!(
            new_dir.join("wal").is_dir(),
            ".lcg/wal/ must be a directory"
        );
        // .lcg/wal/ is a WAL root with one subdirectory per group_id (issue #378): the legacy
        // .graphiti/->.lcg/ migration above lands the WAL files loose at .lcg/wal/'s top level,
        // which the #378 per-group migration then relocates into the default group's
        // subdirectory on the same startup, before the socket is ready (#437: this only works
        // because migrate_workspace runs first — see main.rs).
        let group_wal_dir = new_dir.join("wal").join("liminis");
        for name in wal_file_names {
            assert!(
                group_wal_dir.join(name).exists(),
                "WAL file {name} must be migrated to .lcg/wal/liminis/ (issue #378 WAL-root layout)"
            );
        }
        // FR-002/SC-001: no *.jsonl file may remain loose directly under .lcg/wal/.
        let loose_jsonl: Vec<_> = std::fs::read_dir(new_dir.join("wal"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        assert!(
            loose_jsonl.is_empty(),
            "no *.jsonl file may remain loose directly under .lcg/wal/, found: {loose_jsonl:?}"
        );
        // FR-007/Acceptance Scenario 2: the pre-existing `.wal-generation.json` sidecar (issue
        // #431) must be *relocated*, not stamped over, so its content must survive byte-for-byte
        // under the group directory — checking only existence would also pass for a fresh mint
        // that silently discarded the original legacy generation id.
        let migrated_generation =
            std::fs::read_to_string(group_wal_dir.join(".wal-generation.json"))
                .expect(".wal-generation.json must exist under .lcg/wal/liminis/");
        assert!(
            migrated_generation.contains(legacy_generation_id),
            ".wal-generation.json under .lcg/wal/liminis/ must be the relocated legacy sidecar \
             (containing {legacy_generation_id}), not a freshly minted one — found: \
             {migrated_generation}"
        );
        assert!(
            !new_dir.join("wal").join(".wal-generation.json").exists(),
            ".wal-generation.json must not remain loose at .lcg/wal/"
        );
        assert!(
            new_dir.join("ontology.yaml").exists(),
            "ontology.yaml must be migrated to .lcg/"
        );
        assert!(
            new_dir.join("ontology-hash.json").exists(),
            "ontology-hash.json must be migrated to .lcg/"
        );

        // ── Clean shutdown ────────────────────────────────────────────────────
        send_sigterm(child.id());
        let status = wait_for_exit(&mut child, Duration::from_secs(10));
        let status = match status {
            Some(s) => s,
            None => {
                child.kill().ok();
                panic!("service did not exit within 10s after SIGTERM");
            }
        };
        assert_eq!(
            status.code(),
            Some(0),
            "expected clean exit after migration: {status:?}"
        );
    }

    // Issue #442: regression guard for a `.graphiti`-era workspace started with a *custom*
    // `LCG_WAL_DIR`. Before this fix, `migration::migrate_workspace`'s Step 4 always moved
    // `.graphiti/wal` to the hardcoded `.lcg/wal`, while `wal_group::migrate_wal_root_if_needed`
    // (invoked from `bootstrap_app_state`) scanned the env-resolved `LCG_WAL_DIR` instead — so
    // legacy WAL content ended up loose at `.lcg/wal`, invisible to the running service, which
    // only reads from `<LCG_WAL_DIR>/<group_id>`. This test is the same fixture and assertions
    // as `binary_migrates_legacy_workspace_on_startup` above, but with `LCG_WAL_DIR` set to a
    // path outside `.lcg/` entirely (SC-001, SC-003).
    #[test]
    fn binary_migrates_legacy_workspace_with_custom_lcg_wal_dir_on_startup() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path();
        let (wal_file_names, legacy_generation_id) = build_legacy_workspace_fixture(workspace);
        let legacy = workspace.join(".graphiti");

        // Custom WAL root outside the .lcg/ tree, as an operator placing the WAL on a separate
        // volume might configure. Must be absolute — the binary resolves it CWD-relative
        // otherwise, and cwd is `workspace` here, so a relative custom root would coincidentally
        // still land inside `workspace` but not exercise a genuinely separate volume.
        let custom_wal_root = dir.path().join("custom-wal-root");

        let socket_path = workspace.join(".lcg").join("service.sock");
        let embedder_port = spawn_stub_embedder();
        let embedder_url = format!("http://127.0.0.1:{embedder_port}/v1/embeddings");

        let binary = env!("CARGO_BIN_EXE_liminis-context-graph");
        let mut child = Command::new(binary)
            .current_dir(workspace)
            .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
            .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
            .env("LCG_WAL_DIR", custom_wal_root.to_str().unwrap())
            .args(["--embedder-http", &embedder_url])
            .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
            .spawn()
            .expect("failed to spawn liminis-context-graph");

        let ready = wait_for_socket(&socket_path, Duration::from_secs(30));
        if !ready {
            child.kill().ok();
            panic!("service did not become ready within 30s — migration may have failed");
        }

        // Same blocking IPC round-trip as the default-path test, and for the same reason: proves
        // `bootstrap_app_state` (and thus the per-group WAL migration) has actually completed
        // before the filesystem assertions below run.
        let mut stream =
            UnixStream::connect(&socket_path).expect("failed to connect to service socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"knowledge_status","params":{}}"#;
        writeln!(stream, "{req}").expect("failed to write knowledge_status request");
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("failed to read knowledge_status response");
        let resp: serde_json::Value =
            serde_json::from_str(response.trim()).expect("knowledge_status response is not JSON");
        assert_eq!(
            resp["result"]["context_graph_initialized"],
            serde_json::Value::Bool(true),
            "service must not be in degraded mode after successful migration: {resp}"
        );
        drop(reader);

        // ── Assert legacy content landed under the CUSTOM wal_root, not .lcg/wal ──────────
        assert!(
            !legacy.exists(),
            ".graphiti/ must be removed after migration"
        );
        let group_wal_dir = custom_wal_root.join("liminis");
        for name in wal_file_names {
            assert!(
                group_wal_dir.join(name).exists(),
                "WAL file {name} must be migrated to <LCG_WAL_DIR>/liminis/"
            );
        }
        let migrated_generation =
            std::fs::read_to_string(group_wal_dir.join(".wal-generation.json"))
                .expect(".wal-generation.json must exist under <LCG_WAL_DIR>/liminis/");
        assert!(
            migrated_generation.contains(legacy_generation_id),
            ".wal-generation.json under <LCG_WAL_DIR>/liminis/ must be the relocated legacy \
             sidecar (containing {legacy_generation_id}), not a freshly minted one — found: \
             {migrated_generation}"
        );

        // ── Assert nothing was left loose at either .lcg/wal or the custom root's top level ──
        let lcg_wal_dir = workspace.join(".lcg").join("wal");
        assert!(
            !lcg_wal_dir.exists(),
            ".lcg/wal must not be created when a custom LCG_WAL_DIR is configured, found: \
             {lcg_wal_dir:?}"
        );
        let loose_at_custom_root: Vec<_> = std::fs::read_dir(&custom_wal_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        assert!(
            loose_at_custom_root.is_empty(),
            "no *.jsonl file may remain loose directly under the configured LCG_WAL_DIR, found: \
             {loose_at_custom_root:?}"
        );

        // ── Clean shutdown ────────────────────────────────────────────────────
        send_sigterm(child.id());
        let status = wait_for_exit(&mut child, Duration::from_secs(10));
        let status = match status {
            Some(s) => s,
            None => {
                child.kill().ok();
                panic!("service did not exit within 10s after SIGTERM");
            }
        };
        assert_eq!(
            status.code(),
            Some(0),
            "expected clean exit after migration: {status:?}"
        );
    }
}
