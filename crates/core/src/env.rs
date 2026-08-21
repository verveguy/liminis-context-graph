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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `std::env::set_var`/`remove_var` mutate process-global state, which is unsound to race
    // across threads. Each test below uses its own pair of var names (no two tests share a
    // name), but this lock also serializes against any accidental future overlap.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn lcg_wins_when_both_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("LCG_ENV_TEST_A", "new-value");
        std::env::set_var("GRAPHITI_ENV_TEST_A", "old-value");
        let result = lcg_env_var("LCG_ENV_TEST_A", "GRAPHITI_ENV_TEST_A");
        std::env::remove_var("LCG_ENV_TEST_A");
        std::env::remove_var("GRAPHITI_ENV_TEST_A");
        assert_eq!(result.unwrap(), "new-value");
    }

    #[test]
    fn falls_back_to_deprecated_name_when_new_name_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LCG_ENV_TEST_B");
        std::env::set_var("GRAPHITI_ENV_TEST_B", "old-value");
        let result = lcg_env_var("LCG_ENV_TEST_B", "GRAPHITI_ENV_TEST_B");
        std::env::remove_var("GRAPHITI_ENV_TEST_B");
        assert_eq!(result.unwrap(), "old-value");
    }

    #[test]
    fn errors_when_neither_name_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LCG_ENV_TEST_C");
        std::env::remove_var("GRAPHITI_ENV_TEST_C");
        let result = lcg_env_var("LCG_ENV_TEST_C", "GRAPHITI_ENV_TEST_C");
        assert!(result.is_err());
    }
}
