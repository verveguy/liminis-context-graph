//! `--scope` parsing (FR-004). Scopes are additive/composable, e.g. `read,admin`.
//!
//! `cypher` is a dedicated scope for `knowledge_query_cypher` — a mutation-capable escape
//! hatch that can bypass the WAL/embedding invariants structured writes maintain. It is
//! never implicitly bundled into `read`, `write`, or `admin`; operators must opt in
//! explicitly (or via `all`).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Read,
    Write,
    Cypher,
    Admin,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::Read, Scope::Write, Scope::Cypher, Scope::Admin];

    /// Parses a comma-separated scope list (e.g. `"read,admin"` or `"all"`).
    ///
    /// Returns a deduplicated, order-preserving list. Rejects unknown tokens with the
    /// valid-value list so a typo like `--scope=bogus` fails fast with a clear error.
    pub fn parse_list(s: &str) -> Result<Vec<Scope>, String> {
        let mut scopes: Vec<Scope> = Vec::new();
        let push_unique = |scopes: &mut Vec<Scope>, s: Scope| {
            if !scopes.contains(&s) {
                scopes.push(s);
            }
        };

        for tok in s.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            match tok {
                "all" => {
                    for s in Scope::ALL {
                        push_unique(&mut scopes, s);
                    }
                }
                "read" => push_unique(&mut scopes, Scope::Read),
                "write" => push_unique(&mut scopes, Scope::Write),
                "cypher" => push_unique(&mut scopes, Scope::Cypher),
                "admin" => push_unique(&mut scopes, Scope::Admin),
                other => {
                    return Err(format!(
                        "unknown scope '{other}'; valid values: read, write, cypher, admin, all"
                    ));
                }
            }
        }

        if scopes.is_empty() {
            return Err(
                "--scope requires at least one scope value (read, write, cypher, admin, all)"
                    .to_string(),
            );
        }

        Ok(scopes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_scope() {
        assert_eq!(Scope::parse_list("read").unwrap(), vec![Scope::Read]);
    }

    #[test]
    fn parses_composable_scopes() {
        assert_eq!(
            Scope::parse_list("read,admin").unwrap(),
            vec![Scope::Read, Scope::Admin]
        );
    }

    #[test]
    fn expands_all() {
        assert_eq!(
            Scope::parse_list("all").unwrap(),
            vec![Scope::Read, Scope::Write, Scope::Cypher, Scope::Admin]
        );
    }

    #[test]
    fn dedups_repeated_scopes() {
        assert_eq!(
            Scope::parse_list("read,read,admin").unwrap(),
            vec![Scope::Read, Scope::Admin]
        );
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            Scope::parse_list(" read , admin ").unwrap(),
            vec![Scope::Read, Scope::Admin]
        );
    }

    #[test]
    fn rejects_unknown_scope() {
        let err = Scope::parse_list("bogus").unwrap_err();
        assert!(err.contains("bogus"));
        assert!(err.contains("valid values"));
    }

    #[test]
    fn rejects_empty_string() {
        assert!(Scope::parse_list("").is_err());
    }

    #[test]
    fn cypher_is_not_bundled_into_read_write_or_admin() {
        let scopes = Scope::parse_list("read,write,admin").unwrap();
        assert!(!scopes.contains(&Scope::Cypher));
    }
}
