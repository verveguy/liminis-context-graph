//! Content-addressed embedding cache (issue #440, FR-003/FR-004).
//!
//! Lives entirely in-process memory, keyed by `sha256(model || dim || text)` — FR-003 specifies
//! the cache be keyed by "(source text, embedding model identity)", and the key mixes in both
//! (see `get_or_compute`). Today, in production, exactly one `EmbeddingCache` instance is
//! constructed per running process (see `AppState::embedding_cache`) and is 1:1 with that
//! process's fixed embedder identity, so this never changes today's observable behavior — but a
//! future persisted cache (FR-004 already permits discarding/reloading one) or any other case
//! where one `EmbeddingCache` outlives or is shared across more than one embedder identity would
//! otherwise silently serve one model's vectors to another with nothing to detect it, exactly the
//! failure mode FR-008 exists to eliminate. Mixing identity into the key now, while there is only
//! ever one identity in play, is a size-of-one behavioral no-op that removes that hazard for good.
//!
//! Never written to the WAL or any other durable store (FR-004): losing this cache (process
//! restart, explicit clear) only means the next lookup recomputes at full cost, never an
//! incorrect result.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::embedder::Embedder;
use crate::error::Error;
use crate::replay::RecomputeEmbedFn;

/// Bundles what a WAL-replaying call site needs to recompute embeddings during replay and to
/// record the resulting model identity (issue #440): the running embedder, its model identifier
/// — not obtainable from `Arc<dyn Embedder>` alone, since the `Embedder` trait exposes `dim()`
/// but not a model name — and the shared cache used to bound redundant embedder calls (FR-003).
///
/// Deliberately just data, not behavior: each of the four replay call sites
/// (`Db::open_or_rebuild`, `recovery::run_full_recovery_sequence`,
/// `handlers::handle_rebuild_from_wal`, `handlers::recover_rebuild_from_workspace_wal`) differs
/// in what runtime context it has available to drive
/// `embedder.embed(..)` to completion (a bare-sync caller with no ambient tokio runtime vs. one
/// running inside `tokio::task::spawn_blocking`), so each builds its own `RecomputeEmbedFn`
/// closure around this context rather than sharing one bridging implementation.
#[derive(Clone)]
pub struct EmbedderContext {
    pub embedder: Arc<dyn Embedder>,
    pub model: String,
    pub cache: Arc<EmbeddingCache>,
}

impl EmbedderContext {
    /// `(model, dim)` identity to compare/persist via `WalPositionRecord` (FR-004) — the sole
    /// surviving model-identity mechanism now that the WAL-side stamp is gone (issue #526).
    pub fn identity(&self) -> (String, i64) {
        (self.model.clone(), self.embedder.dim() as i64)
    }

    /// Builds a synchronous `RecomputeEmbedFn` (issue #440) bridging this context's embedder via
    /// `Handle::current().block_on` — documented-safe from inside `tokio::task::spawn_blocking`
    /// (not an async worker thread), per `replay::RecomputeEmbedFn`'s "each caller bridges its
    /// own runtime context" design. Shared by `recovery::run_full_recovery_sequence`,
    /// `handlers::handle_rebuild_from_wal`, and `handlers::recover_rebuild_from_workspace_wal` —
    /// the three replay call sites that always run inside `spawn_blocking`; `Db::open_or_rebuild`'s
    /// bare-sync call site builds its own dedicated runtime instead (see that function), since it
    /// has no ambient one to bridge through.
    ///
    /// Panics (via `Handle::current()`) if called with no ambient tokio runtime — reserved for
    /// those three call sites, never for a bare-sync or test caller.
    pub fn recompute_fn_via_handle(&self) -> RecomputeEmbedFn {
        let handle = tokio::runtime::Handle::current();
        let embedder = Arc::clone(&self.embedder);
        let cache = Arc::clone(&self.cache);
        let (model, dim) = self.identity();
        Box::new(move |text: &str| {
            cache.get_or_compute(&model, dim, text, || handle.block_on(embedder.embed(text)))
        })
    }
}

/// Content-addressed, in-memory-only embedding cache. Safe to share across threads via `Arc`;
/// internally guarded by a single `Mutex` since `AppState.write_lock` already serializes writes
/// across the whole instance (see its doc comment) — a coarse lock here adds no new contention.
///
/// **Known limitation: unbounded growth, no eviction.** `entries` has no capacity cap, TTL, or
/// `clear()` — every distinct `(model, dim, text)` triple embedded across the process's lifetime
/// (every `knowledge_rebuild_from_wal` call, startup recovery, `Db::open_or_rebuild`) accumulates
/// permanently, reclaimed only by a process restart. Acceptable for now: FR-004 requires the
/// cache be safe to discard, not bounded, and it exists to avoid redundant embedder calls within
/// a single rebuild/recovery, not to serve as a long-lived cross-session store. If a deployment's
/// distinct-text pool or rebuild frequency makes this a real memory concern, add a capacity bound
/// (e.g. LRU) — no consumer of `get_or_compute` depends on unbounded retention today.
#[derive(Default)]
pub struct EmbeddingCache {
    entries: Mutex<HashMap<[u8; 32], Vec<f32>>>,
}

impl EmbeddingCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached vector for `(model, dim, text)` if present; otherwise calls `compute`,
    /// stores the result, and returns it. `compute` runs at most once per distinct
    /// `(model, dim, text)` when callers are serialized (SC-005) — true in production, since
    /// `AppState.write_lock` already guarantees at most one write in flight per process. The lock
    /// guarding `entries` is released before `compute` runs (so a slow embed call never blocks
    /// unrelated lookups), which means two callers racing a miss on the *same* key can each run
    /// `compute` once; both results are correct, but the second `insert` silently wins.
    ///
    /// The key mixes `model` and `dim` in ahead of `text`, each separated by a NUL byte, with
    /// `dim` fixed-width (its 8 little-endian bytes) between its two separators — a model name
    /// or text containing an embedded NUL is not expected in practice (both come from embedder
    /// config / extracted content, never attacker-controlled binary data), so this is not
    /// cryptographically collision-proof against adversarial input, but it is sufficient to keep
    /// two distinct, ordinary model identities from ever aliasing the same key (FR-003).
    pub fn get_or_compute(
        &self,
        model: &str,
        dim: i64,
        text: &str,
        compute: impl FnOnce() -> Result<Vec<f32>, Error>,
    ) -> Result<Vec<f32>, Error> {
        let mut hasher = Sha256::new();
        hasher.update(model.as_bytes());
        hasher.update(b"\0");
        hasher.update(dim.to_le_bytes());
        hasher.update(b"\0");
        hasher.update(text.as_bytes());
        let key: [u8; 32] = hasher.finalize().into();

        if let Some(cached) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(cached.clone());
        }

        let vector = compute()?;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, vector.clone());
        Ok(vector)
    }

    /// Number of distinct texts currently cached. Test/diagnostic use.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MODEL_A: &str = "bge-base-en-v1.5";
    const DIM_A: i64 = 768;

    #[test]
    fn cache_hit_avoids_calling_compute() {
        let cache = EmbeddingCache::new();
        let calls = AtomicUsize::new(0);

        let v1 = cache
            .get_or_compute(MODEL_A, DIM_A, "hello", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1.0, 2.0, 3.0])
            })
            .unwrap();
        let v2 = cache
            .get_or_compute(MODEL_A, DIM_A, "hello", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![9.0, 9.0, 9.0])
            })
            .unwrap();

        assert_eq!(v1, vec![1.0, 2.0, 3.0]);
        assert_eq!(v2, vec![1.0, 2.0, 3.0]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn distinct_texts_dont_collide() {
        let cache = EmbeddingCache::new();
        let a = cache
            .get_or_compute(MODEL_A, DIM_A, "a", || Ok(vec![1.0]))
            .unwrap();
        let b = cache
            .get_or_compute(MODEL_A, DIM_A, "b", || Ok(vec![2.0]))
            .unwrap();
        assert_eq!(a, vec![1.0]);
        assert_eq!(b, vec![2.0]);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn compute_error_is_not_cached() {
        let cache = EmbeddingCache::new();
        let calls = AtomicUsize::new(0);

        let first = cache.get_or_compute(MODEL_A, DIM_A, "x", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::Config("boom".to_string()))
        });
        assert!(first.is_err());
        assert!(cache.is_empty());

        let second = cache
            .get_or_compute(MODEL_A, DIM_A, "x", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![5.0])
            })
            .unwrap();
        assert_eq!(second, vec![5.0]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// FR-003 conformance: the cache is keyed by (source text, embedding model identity), not
    /// text alone — two different model identities embedding the same text must never observe
    /// each other's cached vector. This test would fail against the pre-fix key
    /// (`sha256(text)` alone), which is exactly the regression it pins.
    #[test]
    fn different_model_identity_does_not_collide_on_same_text() {
        let cache = EmbeddingCache::new();
        let a = cache
            .get_or_compute("model-a", 768, "shared text", || Ok(vec![1.0, 0.0]))
            .unwrap();
        let b = cache
            .get_or_compute("model-b", 768, "shared text", || Ok(vec![0.0, 1.0]))
            .unwrap();
        assert_eq!(a, vec![1.0, 0.0]);
        assert_eq!(
            b,
            vec![0.0, 1.0],
            "model-b must not reuse model-a's cached entry for the same text"
        );
    }

    /// Same property as above, but for a dimension-only difference under the same model name
    /// (Edge Cases: a dimension override counts as a distinct model identity, not just a
    /// model-name mismatch).
    #[test]
    fn different_dim_does_not_collide_on_same_text_and_model() {
        let cache = EmbeddingCache::new();
        let a = cache
            .get_or_compute(MODEL_A, 768, "shared text", || Ok(vec![1.0]))
            .unwrap();
        let b = cache
            .get_or_compute(MODEL_A, 1024, "shared text", || Ok(vec![2.0]))
            .unwrap();
        assert_eq!(a, vec![1.0]);
        assert_eq!(
            b,
            vec![2.0],
            "a 1024-dim lookup must not reuse the 768-dim cached entry"
        );
    }

    /// Integration-level version of the two key-mixing regressions above, through the same
    /// `EmbedderContext::recompute_fn_via_handle` bridge production actually uses: two
    /// `EmbedderContext`s with different `(model, dim)` sharing one `EmbeddingCache` must not
    /// observe each other's vectors for the same source text.
    #[tokio::test]
    async fn embedder_contexts_with_different_identity_sharing_one_cache_do_not_collide() {
        let cache = Arc::new(EmbeddingCache::new());

        let mut map_a = StdHashMap::new();
        map_a.insert("shared text".to_string(), vec![1.0, 0.0]);
        let ctx_a = EmbedderContext {
            embedder: Arc::new(crate::embedder::NameMapEmbedder::new(2, map_a)),
            model: "model-a".to_string(),
            cache: Arc::clone(&cache),
        };

        let mut map_b = StdHashMap::new();
        map_b.insert("shared text".to_string(), vec![0.0, 1.0]);
        let ctx_b = EmbedderContext {
            embedder: Arc::new(crate::embedder::NameMapEmbedder::new(2, map_b)),
            model: "model-b".to_string(),
            cache: Arc::clone(&cache),
        };

        // recompute_fn_via_handle's Handle::current().block_on bridge must run from inside
        // spawn_blocking, matching every production call site (never from the async task
        // itself, which would panic trying to nest runtimes).
        let (va, vb) = tokio::task::spawn_blocking(move || {
            let fn_a = ctx_a.recompute_fn_via_handle();
            let fn_b = ctx_b.recompute_fn_via_handle();
            let va = fn_a("shared text").unwrap();
            let vb = fn_b("shared text").unwrap();
            (va, vb)
        })
        .await
        .unwrap();

        assert_eq!(va, vec![1.0, 0.0]);
        assert_eq!(
            vb,
            vec![0.0, 1.0],
            "ctx_b must not observe ctx_a's cached vector for the same text"
        );
    }
}
