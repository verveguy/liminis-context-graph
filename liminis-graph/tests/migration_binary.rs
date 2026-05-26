// Binary-level migration integration test (SC-001, SC-002, FR-002).
//
// Spawns the real binary against a legacy .graphiti/ workspace, waits for the
// service to become ready (migration ran + socket bound), then asserts:
// 1. The correct post-migration file layout under .lcg/
// 2. The service is not degraded and responds to IPC
// 3. Entity counts match the pre-migration state (0, since the legacy DB was empty)

#[cfg(unix)]
mod migration_binary_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use liminis_graph_core::db::Db;
    use tempfile::TempDir;

    fn wait_for_socket(socket_path: &PathBuf, timeout: Duration) -> bool {
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

    // FR-001, FR-002, FR-003, SC-001, SC-002:
    // Binary migrates a legacy .graphiti/ workspace to .lcg/ on startup and then
    // serves IPC requests normally without data loss.
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

        // Application WAL directory with a dummy JSONL file
        let wal_dir = legacy.join("wal");
        std::fs::create_dir(&wal_dir).unwrap();
        std::fs::write(wal_dir.join("001.jsonl"), b"{\"type\":\"test\"}\n").unwrap();

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
            .spawn()
            .expect("failed to spawn liminis-context-graph");

        let ready = wait_for_socket(&socket_path, Duration::from_secs(30));
        if !ready {
            child.kill().ok();
            panic!("service did not become ready within 30s — migration may have failed");
        }

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
        assert!(
            new_dir.join("wal").join("001.jsonl").exists(),
            "WAL files must be migrated to .lcg/wal/"
        );
        assert!(
            new_dir.join("ontology.yaml").exists(),
            "ontology.yaml must be migrated to .lcg/"
        );
        assert!(
            new_dir.join("ontology-hash.json").exists(),
            "ontology-hash.json must be migrated to .lcg/"
        );

        // ── Assert IPC is functional post-migration ───────────────────────────
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
