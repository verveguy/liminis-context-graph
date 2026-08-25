// Integration test: SIGTERM → clean exit → clean DB re-open (R2, R3, R5).
//
// Spawns the binary, waits for it to be ready, sends a knowledge_build_indices
// request to write something to the LadybugDB WAL, then sends SIGTERM and verifies:
// 1. The process exits with code 0 (not killed by signal).
// 2. The DB can be re-opened without "Corrupted wal file" — the WAL was checkpointed.

#[cfg(unix)]
#[path = "common/mod.rs"]
mod common;

#[cfg(unix)]
mod clean_shutdown_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use lcg_core::db::Db;
    use tempfile::TempDir;

    use super::common::ChildGuard;

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

    /// Sends SIGTERM directly from this test process (rather than shelling out to `kill`), so
    /// the known sender PID for FR-008's assertion is trivially this process's own PID (#247).
    fn send_sigterm(pid: u32) {
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(
            rc,
            0,
            "libc::kill failed: {}",
            std::io::Error::last_os_error()
        );
    }

    /// Spawns a background thread that reads `stderr` line-by-line and returns a join handle
    /// yielding every captured line once the pipe reaches EOF (i.e. after the child exits).
    fn spawn_stderr_reader(
        stderr: std::process::ChildStderr,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => lines.push(line),
                }
            }
            lines
        })
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
        let mut cmd = Command::new(binary);
        cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
            .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
            // Short shutdown timeout so the test finishes quickly.
            .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
            // Provide a stub embedder so the startup probe succeeds on CI
            // (no Swift sidecar or LCG_EMBEDDING_URL available in the test environment).
            // Also provide an explicit extractor endpoint: with no ANTHROPIC_API_KEY, no
            // sidecar socket, and no LCG_EXTRACTION_URL in the CI environment, startup would
            // otherwise fail fast per FR-011. The test never exercises extraction (only
            // knowledge_build_indices), so this URL need not be reachable.
            .args(["--embedder-http", &embedder_url])
            .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
            .stderr(Stdio::piped());
        // ChildGuard ensures the spawned process is killed and reaped even if this test panics
        // or fails an assertion before reaching its own explicit SIGTERM/wait sequence (#500).
        let mut child = ChildGuard::spawn(cmd);
        let stderr_handle = spawn_stderr_reader(child.stderr.take().expect("child stderr"));

        let ready = wait_for_socket(&socket_path, Duration::from_secs(15));
        if !ready {
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

        // Send SIGTERM directly from this test process — the service should checkpoint the WAL
        // and exit cleanly, and the known sender PID is this process's own PID (#247/FR-008).
        let sender_pid = std::process::id();
        send_sigterm(child.id());

        let status = wait_for_exit(&mut child, Duration::from_secs(10))
            .unwrap_or_else(|| panic!("service did not exit within 10s after SIGTERM"));

        assert_eq!(
            status.code(),
            Some(0),
            "expected exit code 0 after SIGTERM, got: {status:?}"
        );

        let stderr_lines = stderr_handle.join().expect("stderr reader thread panicked");
        let expected = format!("sender pid={sender_pid}");
        assert!(
            stderr_lines.iter().any(|line| line.contains(&expected)),
            "expected stderr to contain {expected:?}, got: {stderr_lines:?}"
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
