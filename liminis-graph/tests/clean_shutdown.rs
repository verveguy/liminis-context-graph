// Integration test: SIGTERM → clean exit → clean DB re-open (R2, R3, R5).
//
// Spawns the binary, waits for it to be ready, sends a knowledge_build_indices
// request to write something to the LadybugDB WAL, then sends SIGTERM and verifies:
// 1. The process exits with code 0 (not killed by signal).
// 2. The DB can be re-opened without "Corrupted wal file" — the WAL was checkpointed.

#[cfg(unix)]
mod clean_shutdown_tests {
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

    /// Spawns a minimal stub HTTP embedder on a random OS-assigned port.
    /// Returns the port. The stub serves one valid OAI embedding response per connection.
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
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = Read::read(&mut s, &mut buf);
                let _ = Write::write_all(&mut s, http_response.as_bytes());
            }
        });

        port
    }

    #[test]
    fn sigterm_produces_clean_exit_and_no_wal_corruption() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let socket_path = dir.path().join("service.sock");

        let embedder_port = spawn_stub_embedder();
        let embedder_url = format!("http://127.0.0.1:{embedder_port}/v1/embeddings");

        let binary = env!("CARGO_BIN_EXE_liminis-context-graph");
        let mut child = Command::new(binary)
            .env("LCG_DB_PATH", db_path.to_str().unwrap())
            .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
            // Short shutdown timeout so the test finishes quickly.
            .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
            // Provide a stub embedder so the startup probe succeeds on CI
            // (no Swift sidecar or LCG_EMBEDDING_URL available in the test environment).
            .args(["--embedder-http", &embedder_url])
            .spawn()
            .expect("failed to spawn liminis-context-graph");

        let ready = wait_for_socket(&socket_path, Duration::from_secs(15));
        if !ready {
            child.kill().ok();
            panic!("service did not become ready within 15s");
        }

        // Send knowledge_build_indices to write to the LadybugDB WAL.
        let mut stream =
            UnixStream::connect(&socket_path).expect("failed to connect to service socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"knowledge_build_indices","params":{}}"#;
        writeln!(stream, "{request}").expect("failed to write request");

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("failed to read response");
        assert!(
            !response.is_empty(),
            "expected a response from knowledge_build_indices"
        );

        drop(reader);

        // Send SIGTERM — the service should checkpoint the WAL and exit cleanly.
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
            "expected exit code 0 after SIGTERM, got: {status:?}"
        );

        // Verify no WAL corruption: re-open the DB that the service just closed.
        let db_result = Db::open(db_path.to_str().unwrap());
        assert!(
            db_result.is_ok(),
            "DB re-open failed after clean shutdown — possible WAL corruption: {:?}",
            db_result.err()
        );
    }
}
