/// Phase A two-name env var lookup.
///
/// Tries `new_name` first; if absent falls back to `old_name` with a deprecation warning.
/// Each call site is annotated `// deprecated: remove in Phase B (see #59)`.
pub fn lcg_env_var(new_name: &str, old_name: &str) -> Result<String, std::env::VarError> {
    match std::env::var(new_name) {
        Ok(v) => Ok(v),
        Err(std::env::VarError::NotPresent) => match std::env::var(old_name) {
            Ok(v) => {
                eprintln!(
                    "[liminis-context-graph] DEPRECATED: env var {old_name} is deprecated; \
                     rename to {new_name}. Support will be removed in Phase B (see issue #59)."
                );
                Ok(v)
            }
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}
