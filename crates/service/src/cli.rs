//! Pure, unit-testable CLI argument parsing for the `liminis-context-graph` binary.
//!
//! Kept as a hand-rolled scan (matching the pre-existing `--embedder-uds`/`--embedder-http`
//! pattern in `main.rs`) rather than adopting `clap`: a handful of flags doesn't clear the bar
//! for a new dependency, and a pure `parse_args` function is just as testable.

use crate::mcp::scope::Scope;

#[derive(Debug, Clone, PartialEq)]
pub enum EmbedderFlag {
    Uds(String),
    Http(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CliMode {
    /// Default: run the Unix-socket JSON-RPC service (existing behavior).
    Socket { embedder: Option<EmbedderFlag> },
    /// `--mcp-stdio`: run an MCP server over stdin/stdout (FR-001).
    Mcp {
        embedder: Option<EmbedderFlag>,
        /// `--connect <path>`: attached mode — forward calls to a running service instead
        /// of opening the DB directly (FR-006).
        connect: Option<String>,
        scopes: Vec<Scope>,
        /// `--allow-remote-close`: only meaningful in attached mode (FR-005).
        allow_remote_close: bool,
    },
    /// `--help` / `-h`: caller should print `usage()` and exit successfully.
    Help,
    /// `--version` / `-V`: caller should print the version and exit successfully.
    Version,
}

/// Usage text for `--help` and for the error message on an unrecognized argument. Kept in one
/// place so the CLI is self-documenting — running the binary is otherwise the only way to learn
/// the flags exist (`--mcp-stdio`, `--scope`, `--connect`, …), and they were previously
/// undiscoverable without reading the README.
pub fn usage() -> &'static str {
    "liminis-context-graph — local-first context graph engine

USAGE:
    liminis-context-graph [OPTIONS]

By default, runs the Unix-socket JSON-RPC service (binds .lcg/service.sock in the current
workspace). Pass --mcp-stdio to run as an MCP server over stdin/stdout instead.

OPTIONS:
    --mcp-stdio                Run as an MCP (Model Context Protocol) server over stdin/stdout
                               instead of binding the Unix socket.
    --scope <LIST>             (MCP only) Comma-separated tool scopes to advertise in
                               tools/list: read, write, cypher, admin, all. Default: all.
    --connect <SOCKET_PATH>    (MCP only) Attached mode: forward each call over this socket to
                               an already-running service instead of opening the database.
    --allow-remote-close       (MCP attached mode only) Allow knowledge_close to shut down the
                               remote service. No effect in standalone mode.
    --embedder-uds <SOCKET>    Reach the embedding sidecar over this Unix socket.
    --embedder-http <URL>      Reach the embedding sidecar over this HTTP URL.
                               (--embedder-uds and --embedder-http are mutually exclusive.)
    -h, --help                 Print this help and exit.
    -V, --version              Print version and exit.

Documentation: https://github.com/verveguy/liminis-context-graph
"
}

/// Parses argv (excluding the program name at index 0).
pub fn parse_args(args: &[String]) -> Result<CliMode, String> {
    let mut mcp_stdio = false;
    let mut connect: Option<String> = None;
    let mut scope_arg: Option<String> = None;
    let mut allow_remote_close = false;
    let mut cli_uds: Option<String> = None;
    let mut cli_http: Option<String> = None;
    let mut want_help = false;
    let mut want_version = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => want_help = true,
            "--version" | "-V" => want_version = true,
            "--mcp-stdio" => mcp_stdio = true,
            "--allow-remote-close" => allow_remote_close = true,
            "--connect" => {
                i += 1;
                connect = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--connect requires a socket path argument")?,
                );
            }
            "--embedder-uds" => {
                i += 1;
                cli_uds = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--embedder-uds requires a socket path argument")?,
                );
            }
            "--embedder-http" => {
                i += 1;
                cli_http = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--embedder-http requires a URL argument")?,
                );
            }
            "--scope" => {
                i += 1;
                scope_arg = Some(args.get(i).cloned().ok_or("--scope requires a value")?);
            }
            arg => {
                if let Some(v) = arg.strip_prefix("--scope=") {
                    scope_arg = Some(v.to_string());
                } else {
                    // Previously unknown arguments were silently ignored, so a typo like
                    // `--scoep=read` ran with default scope with no warning. Reject them.
                    return Err(format!("unknown argument: '{arg}'\n\n{}", usage()));
                }
            }
        }
        i += 1;
    }

    // --help / --version short-circuit everything else (including the validation below), so
    // `--help` prints usage even alongside otherwise-invalid flag combinations. Help wins over
    // version when both are present.
    if want_help {
        return Ok(CliMode::Help);
    }
    if want_version {
        return Ok(CliMode::Version);
    }

    if cli_uds.is_some() && cli_http.is_some() {
        return Err(
            "--embedder-uds and --embedder-http are mutually exclusive; specify only one"
                .to_string(),
        );
    }
    let embedder = match (cli_uds, cli_http) {
        (Some(u), _) => Some(EmbedderFlag::Uds(u)),
        (_, Some(h)) => Some(EmbedderFlag::Http(h)),
        _ => None,
    };

    if !mcp_stdio {
        if connect.is_some() || scope_arg.is_some() {
            return Err("--connect and --scope require --mcp-stdio".to_string());
        }
        // --allow-remote-close only affects attached MCP mode; silently accepted elsewhere
        // per the spec's explicit "no effect" edge case rather than erroring.
        return Ok(CliMode::Socket { embedder });
    }

    if allow_remote_close && connect.is_none() {
        eprintln!(
            "liminis-context-graph: --allow-remote-close has no effect in standalone MCP mode \
             (no --connect); ignoring"
        );
    }

    let scopes = Scope::parse_list(scope_arg.as_deref().unwrap_or("all"))?;

    Ok(CliMode::Mcp {
        embedder,
        connect,
        scopes,
        allow_remote_close,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_to_socket_mode() {
        assert_eq!(
            parse_args(&args(&[])).unwrap(),
            CliMode::Socket { embedder: None }
        );
    }

    #[test]
    fn socket_mode_with_embedder_uds() {
        assert_eq!(
            parse_args(&args(&["--embedder-uds", "/tmp/x.sock"])).unwrap(),
            CliMode::Socket {
                embedder: Some(EmbedderFlag::Uds("/tmp/x.sock".to_string()))
            }
        );
    }

    #[test]
    fn embedder_uds_and_http_are_mutually_exclusive() {
        let err = parse_args(&args(&[
            "--embedder-uds",
            "/tmp/x.sock",
            "--embedder-http",
            "http://x",
        ]))
        .unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn mcp_stdio_defaults_to_all_scope_standalone() {
        match parse_args(&args(&["--mcp-stdio"])).unwrap() {
            CliMode::Mcp {
                connect,
                scopes,
                allow_remote_close,
                embedder,
            } => {
                assert_eq!(connect, None);
                assert_eq!(scopes, Scope::ALL.to_vec());
                assert!(!allow_remote_close);
                assert_eq!(embedder, None);
            }
            other => panic!("expected Mcp mode, got {other:?}"),
        }
    }

    #[test]
    fn mcp_stdio_with_scope_flag_equals_form() {
        match parse_args(&args(&["--mcp-stdio", "--scope=read,admin"])).unwrap() {
            CliMode::Mcp { scopes, .. } => {
                assert_eq!(scopes, vec![Scope::Read, Scope::Admin]);
            }
            other => panic!("expected Mcp mode, got {other:?}"),
        }
    }

    #[test]
    fn mcp_stdio_with_scope_flag_space_form() {
        match parse_args(&args(&["--mcp-stdio", "--scope", "cypher"])).unwrap() {
            CliMode::Mcp { scopes, .. } => {
                assert_eq!(scopes, vec![Scope::Cypher]);
            }
            other => panic!("expected Mcp mode, got {other:?}"),
        }
    }

    #[test]
    fn mcp_stdio_attached_mode() {
        match parse_args(&args(&["--mcp-stdio", "--connect", ".lcg/service.sock"])).unwrap() {
            CliMode::Mcp { connect, .. } => {
                assert_eq!(connect, Some(".lcg/service.sock".to_string()));
            }
            other => panic!("expected Mcp mode, got {other:?}"),
        }
    }

    #[test]
    fn mcp_stdio_attached_mode_with_allow_remote_close() {
        match parse_args(&args(&[
            "--mcp-stdio",
            "--connect",
            ".lcg/service.sock",
            "--allow-remote-close",
        ]))
        .unwrap()
        {
            CliMode::Mcp {
                allow_remote_close, ..
            } => assert!(allow_remote_close),
            other => panic!("expected Mcp mode, got {other:?}"),
        }
    }

    #[test]
    fn allow_remote_close_without_connect_is_accepted_but_inert() {
        // Edge case from the spec: no error, just a no-op (stderr notice only).
        let result = parse_args(&args(&["--mcp-stdio", "--allow-remote-close"]));
        assert!(result.is_ok());
    }

    #[test]
    fn connect_without_mcp_stdio_is_rejected() {
        let err = parse_args(&args(&["--connect", ".lcg/service.sock"])).unwrap_err();
        assert!(err.contains("--mcp-stdio"));
    }

    #[test]
    fn scope_without_mcp_stdio_is_rejected() {
        let err = parse_args(&args(&["--scope=read"])).unwrap_err();
        assert!(err.contains("--mcp-stdio"));
    }

    #[test]
    fn unknown_scope_is_rejected() {
        let err = parse_args(&args(&["--mcp-stdio", "--scope=bogus"])).unwrap_err();
        assert!(err.contains("bogus"));
    }

    #[test]
    fn help_flag_is_recognized() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), CliMode::Help);
        assert_eq!(parse_args(&args(&["-h"])).unwrap(), CliMode::Help);
    }

    #[test]
    fn version_flag_is_recognized() {
        assert_eq!(parse_args(&args(&["--version"])).unwrap(), CliMode::Version);
        assert_eq!(parse_args(&args(&["-V"])).unwrap(), CliMode::Version);
    }

    #[test]
    fn help_wins_over_version_and_other_flags() {
        // Help short-circuits even alongside otherwise-invalid combinations.
        assert_eq!(
            parse_args(&args(&["--version", "--help"])).unwrap(),
            CliMode::Help
        );
        assert_eq!(
            parse_args(&args(&["--mcp-stdio", "--help"])).unwrap(),
            CliMode::Help
        );
    }

    #[test]
    fn unknown_flag_is_rejected_with_usage() {
        // Previously silently ignored; a typo must now surface as an error carrying usage text.
        let err = parse_args(&args(&["--scoep=read"])).unwrap_err();
        assert!(err.contains("unknown argument"));
        assert!(err.contains("--scoep=read"));
        assert!(err.contains("USAGE:"));
    }

    #[test]
    fn known_embedder_flag_still_accepted_after_unknown_rejection() {
        // Guards the app's invocation contract: it spawns the binary with `--embedder-uds`.
        assert_eq!(
            parse_args(&args(&["--embedder-uds", "/tmp/x.sock"])).unwrap(),
            CliMode::Socket {
                embedder: Some(EmbedderFlag::Uds("/tmp/x.sock".to_string()))
            }
        );
    }

    #[test]
    fn usage_text_lists_the_key_flags() {
        let u = usage();
        for flag in [
            "--mcp-stdio",
            "--scope",
            "--connect",
            "--embedder-uds",
            "--help",
        ] {
            assert!(u.contains(flag), "usage missing {flag}");
        }
    }
}
