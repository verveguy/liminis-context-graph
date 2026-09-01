use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Error;

/// Metadata captured at the moment a WAL file is closed by rotation.
#[derive(Debug, Clone)]
pub struct WalRotationInfo {
    pub from_file_seq: u32,
    pub to_file_seq: u32,
    pub closed_bytes: u64,
    pub closed_events: usize,
}

/// Embedding vector param keys that never belong in a durable WAL record (issue #526, FR-001) —
/// a vector is a local cache of the database, always recomputed from co-located source text on
/// replay (`replay::EMBEDDING_TEXT_PAIRS`), never content of the log itself. Stripped from every
/// mutation's params in [`WalWriter::log_mutation`] before it is ever written to disk, regardless
/// of write path — the live-write drain (`wal_exec.rs`) and `dump.rs`'s direct `log_mutation`
/// calls both funnel through this one function, so there is no second strip site to keep in sync.
///
/// Kept in sync with `replay::EMBEDDING_TEXT_PAIRS`'s key set by a same-crate test
/// (`replay::tests::embedding_text_pairs_key_set_matches_wal_strip_list`) — the two lists must
/// never drift apart, since a key present in one but not the other would either leave a vector
/// un-stripped here or leave replay unable to recognize a vector this writer no longer emits.
pub(crate) const VECTOR_PARAM_KEYS: &[&str] = &[
    "name_embedding",
    "fact_embedding",
    "content_embedding",
    "summary_embedding",
];

/// One WAL record — five-field JSONL schema matching the Python `graphiti_core/driver/wal.py`.
/// Fields are declared in `seq, ts, db, cypher, params` order; serde_json preserves
/// struct field declaration order, matching Python's `json.dumps()` dict insertion order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalLine {
    pub seq: u64,
    pub ts: String,
    pub db: String,
    pub cypher: String,
    pub params: serde_json::Value,
}

/// Appends WAL lines atomically per chunk to the configured WAL dir (typically `.lcg/wal/`).
pub struct WalWriter {
    wal_dir: PathBuf,
    global_seq: u64,
    file_seq: u32,
    events_in_current_file: usize,
    max_events_per_file: usize,
    max_bytes_per_file: u64,
    bytes_in_current_file: u64,
    session_id: String,
    pending_lines: Vec<WalLine>,
    current_file: Option<PathBuf>,
    last_rotation: Option<WalRotationInfo>,
    /// This stream's generation identity (issue #387), read once at construction time and
    /// cached for the life of this writer — never re-read per chunk, so the highest-frequency
    /// write path in the system (`with_chunk`/`flush_pending`) pays no extra filesystem I/O.
    /// `None` only for a pre-existing, pre-#387 populated stream that has never had a
    /// generation minted into it (Story 5) — never retroactively backfilled here.
    generation: Option<String>,
}

impl WalWriter {
    /// Opens (or creates) the WAL directory and scans existing files to determine the
    /// starting global sequence number.
    ///
    /// `max_bytes_per_file = 0` disables byte-size rotation (only event-count applies).
    ///
    /// Also establishes this stream's generation identity (issue #387): if a generation is
    /// already recorded, it's read as-is; if the directory has no prior `.jsonl` content
    /// (`global_seq == 0`) and no generation record yet, a fresh one is minted here — the same
    /// "no existing content" branch that creates the directory in the first place, which is why
    /// `knowledge_dump_wal`'s always-fresh `target_dir` automatically gets a new generation with
    /// no dump-specific code (FR-006). A pre-existing populated directory with no generation
    /// record is left as `None` — this issue does not retroactively mint one for legacy streams.
    pub fn new(
        wal_dir: impl Into<PathBuf>,
        max_events_per_file: usize,
        max_bytes_per_file: u64,
    ) -> Result<Self, Error> {
        let wal_dir = wal_dir.into();
        fs::create_dir_all(&wal_dir)?;

        let global_seq = scan_max_seq(&wal_dir)?;
        let session_id = Uuid::new_v4()
            .as_simple()
            .to_string()
            .chars()
            .take(6)
            .collect::<String>();

        let generation = match crate::wal_generation::read_generation(&wal_dir) {
            Some(g) => Some(g),
            None if global_seq == 0 => Some(crate::wal_generation::ensure_generation(&wal_dir)?),
            None => None,
        };

        Ok(Self {
            wal_dir,
            global_seq,
            file_seq: 0,
            events_in_current_file: 0,
            max_events_per_file,
            max_bytes_per_file,
            bytes_in_current_file: 0,
            session_id,
            pending_lines: Vec::new(),
            current_file: None,
            last_rotation: None,
            generation,
        })
    }

    /// This stream's generation identity (issue #387), cached at construction time. `None` only
    /// for a pre-existing stream that predates this feature and has never had one recorded.
    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }

    /// Buffers a mutation. Filters out reads and index DDL; must be called inside `with_chunk`.
    ///
    /// `params` has every [`VECTOR_PARAM_KEYS`] entry stripped before the line is buffered
    /// (issue #526, FR-001) — a vector is a local cache, recomputed from co-located source text
    /// on replay, never durable log content.
    pub fn log_mutation(
        &mut self,
        cypher: &str,
        params: serde_json::Value,
        database: &str,
    ) -> Result<(), Error> {
        // Filter index DDL before first-token check (higher priority per AD-W7).
        if is_index_ddl(cypher) {
            return Ok(());
        }

        if !looks_like_mutation(cypher) {
            return Ok(());
        }

        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string();
        let line = WalLine {
            seq: self.global_seq,
            ts,
            db: database.to_string(),
            cypher: cypher.to_string(),
            params: strip_vector_params(params),
        };
        self.global_seq += 1;
        self.pending_lines.push(line);
        Ok(())
    }

    /// Chunk-atomic write: runs `f`; on `Ok` flushes pending lines to one file; on `Err`
    /// discards the buffer (R-02 invariant).
    pub fn with_chunk<F, T>(&mut self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut WalWriter) -> Result<T, Error>,
    {
        self.pending_lines.clear();
        let result = f(self);
        match result {
            Ok(val) => {
                self.flush_pending()?;
                Ok(val)
            }
            Err(e) => {
                self.pending_lines.clear();
                Err(e)
            }
        }
    }

    fn flush_pending(&mut self) -> Result<(), Error> {
        let chunk_len = self.pending_lines.len();
        if chunk_len == 0 {
            return Ok(());
        }

        fs::create_dir_all(&self.wal_dir)?;

        // Pre-serialize all lines so we can compute chunk_bytes for byte-size rotation.
        let jsons: Vec<String> = self
            .pending_lines
            .iter()
            .map(|l| serde_json::to_string(l).map_err(|e| Error::WalJson(e.to_string())))
            .collect::<Result<_, _>>()?;
        // Each line occupies json.len() + 1 bytes (the '\n').
        let chunk_bytes: u64 = jsons.iter().map(|s| (s.len() + 1) as u64).sum();

        if self.max_bytes_per_file > 0 && chunk_bytes > self.max_bytes_per_file {
            eprintln!(
                "[WAL WARN] chunk ({chunk_bytes} bytes) exceeds max_bytes_per_file ({}); writing anyway",
                self.max_bytes_per_file
            );
        }

        // Rotate if: no file open, event count would exceed max, or byte size would exceed max.
        let needs_new_file = self.current_file.is_none()
            || (self.events_in_current_file > 0
                && self.events_in_current_file + chunk_len > self.max_events_per_file)
            || (self.max_bytes_per_file > 0
                && self.bytes_in_current_file > 0
                && self.bytes_in_current_file + chunk_bytes > self.max_bytes_per_file);

        if needs_new_file {
            // Capture rotation info before resetting counters.
            if self.current_file.is_some() {
                self.last_rotation = Some(WalRotationInfo {
                    from_file_seq: self.file_seq - 1,
                    to_file_seq: self.file_seq,
                    closed_bytes: self.bytes_in_current_file,
                    closed_events: self.events_in_current_file,
                });
            }
            self.current_file = Some(self.make_new_file_path());
            self.events_in_current_file = 0;
            self.bytes_in_current_file = 0;
        }

        let path = self.current_file.as_ref().unwrap();
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = BufWriter::new(file);
        for json in &jsons {
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;

        self.events_in_current_file += chunk_len;
        self.bytes_in_current_file += chunk_bytes;
        self.pending_lines.clear();
        Ok(())
    }

    fn make_new_file_path(&mut self) -> PathBuf {
        let now = Utc::now();
        let path = self.wal_dir.join(format!(
            "{}_{}_{:04}.jsonl",
            now.format("%Y%m%d_%H%M%S"),
            self.session_id,
            self.file_seq,
        ));
        self.file_seq += 1;
        path
    }

    /// Force-closes the current WAL file (if open) so the next write opens a fresh file.
    /// Since writes are flushed and fsynced per chunk, rotation only resets `current_file`.
    /// Returns `(files_rotated, files_total)` — `files_rotated` is 0 or 1.
    pub fn rotate(&mut self) -> (u32, u32) {
        let files_rotated = if self.current_file.take().is_some() {
            self.last_rotation = Some(WalRotationInfo {
                from_file_seq: self.file_seq - 1,
                to_file_seq: self.file_seq,
                closed_bytes: self.bytes_in_current_file,
                closed_events: self.events_in_current_file,
            });
            self.bytes_in_current_file = 0;
            self.events_in_current_file = 0;
            1
        } else {
            0
        };
        let files_total = count_jsonl_files(&self.wal_dir);
        (files_rotated, files_total)
    }

    /// Drains and returns rotation info if a rotation occurred since the last call.
    /// Returns `None` if no rotation has happened. Intended to be called after `with_chunk`.
    pub fn take_rotation(&mut self) -> Option<WalRotationInfo> {
        self.last_rotation.take()
    }

    /// Returns pending line count (for tests).
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending_lines.len()
    }

    /// Re-derives `global_seq` after a rebuild/clear that may have replayed a WAL directory
    /// populated after this writer was constructed (issue #352). Combines a fresh on-disk scan
    /// (the same `scan_max_seq` used at startup) with the replay's own `last_committed_seq` —
    /// a failed-to-replay line can still have a higher on-disk `seq` than the last commit, so
    /// neither source alone is trustworthy (FR-003). Monotonic: only ever raises `global_seq`
    /// (`max(current, scanned, last_committed_seq + 1)`), never lowers it (FR-005), so this is a
    /// safe no-op when called over an empty or low-max-seq WAL directory.
    pub fn resync_global_seq(&mut self, last_committed_seq: Option<u64>) -> Result<(), Error> {
        // Apply the last_committed_seq floor first: it requires no I/O and is already the most
        // trustworthy source available (it came directly from the replay that just ran). If the
        // on-disk scan below fails, this floor still stands — a scan failure only forfeits the
        // scan's extra precision, not the guaranteed floor.
        let from_commit = last_committed_seq.map(|s| s.saturating_add(1)).unwrap_or(0);
        self.global_seq = self.global_seq.max(from_commit);
        let scanned = scan_max_seq(&self.wal_dir)?;
        self.global_seq = self.global_seq.max(scanned);
        Ok(())
    }

    /// Returns the current `global_seq` — the next seq to be assigned, i.e. one past the
    /// highest seq actually written so far. Used by `wal_exec::wal_flush_chunk` (issue #353)
    /// to compute the max seq assigned during a chunk via a before/after diff. Crate-visible
    /// only — `WalWriter` is re-exported from the crate root, and nothing outside `lcg-core`
    /// needs this, so `pub(crate)` avoids committing to it as public API.
    pub(crate) fn global_seq(&self) -> u64 {
        self.global_seq
    }
}

fn count_jsonl_files(dir: &Path) -> u32 {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .count() as u32
        })
        .unwrap_or(0)
}

/// Reads all `.jsonl` files in `wal_dir` and returns the literal highest seq present. Used only
/// by `scan_max_seq` (`WalWriter::new`/`resync_global_seq`'s hot startup/resync path), which
/// needs nothing beyond the plain value — unlike `wal_max_seq`'s fallback, which needs the fuller
/// [`ScanWalBoundsDetail`] to populate the manifest. Tolerates truncated final lines. Visits every
/// file regardless of iteration order, so — unlike a filename-sorted approximation — this is
/// correct even when the lexicographically-first/last file doesn't hold the true min/max seq
/// (clock skew, concurrent writers sharing a directory).
fn scan_max_seq_detailed(wal_dir: &Path) -> Result<Option<u64>, Error> {
    let files: Vec<PathBuf> = fs::read_dir(wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();

    let mut max_seq: Option<u64> = None;
    for path in &files {
        if let Some(seq) = read_last_seq(path)? {
            if max_seq.is_none_or(|m| seq > m) {
                max_seq = Some(seq);
            }
        }
    }

    Ok(max_seq)
}

/// Fingerprint of a `.jsonl` file *name set* (order-independent — the input is sorted before
/// hashing), used as a cheap (no file opens, just names already in hand from one `read_dir`)
/// staleness signal that changes on ANY membership change: a file added, removed, or renamed.
///
/// A plain file *count* is not sufficient here: a same-count swap (one file removed, a
/// different one added — e.g. an external prune paired with a new rotation) leaves the count
/// unchanged even though the true max-seq-holding file may have changed, which a count-only
/// check would silently miss (see FR-008 in issue #375 — the fast path MUST still return the
/// true max after any out-of-band directory modification, not just after a size change).
fn jsonl_file_set_fingerprint<P: AsRef<Path>>(paths: &[P]) -> u64 {
    let mut names: Vec<_> = paths
        .iter()
        .filter_map(|p| p.as_ref().file_name().map(|n| n.to_owned()))
        .collect();
    names.sort_unstable();
    let mut hasher = DefaultHasher::new();
    names.hash(&mut hasher);
    hasher.finish()
}

/// Same as [`jsonl_file_set_fingerprint`] but reads the current file name set from disk with a
/// single non-recursive `read_dir` pass (no file opens) — the fast-path staleness check doesn't
/// have an already-collected file list to reuse the way `scan_max_seq_detailed` does. Returns
/// `None` if the directory can't be listed at all — that must be treated as "can't confirm
/// freshness," not silently coerced into an empty file set, or an unreadable directory could
/// spuriously "match" an empty-WAL manifest and serve a cached value past a real error.
fn current_jsonl_file_set_fingerprint(wal_dir: &Path) -> Option<u64> {
    let names: Vec<PathBuf> = fs::read_dir(wal_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    Some(jsonl_file_set_fingerprint(&names))
}

/// Reads all `.jsonl` files in `wal_dir` and returns `max_seq + 1`, or 0 if no lines are found.
fn scan_max_seq(wal_dir: &Path) -> Result<u64, Error> {
    Ok(scan_max_seq_detailed(wal_dir)?.map(|s| s + 1).unwrap_or(0))
}

/// Result of a full directory scan for *both* WAL seq bounds, enough to populate a complete
/// [`WalBoundsManifest`] in one pass. Used by `wal_max_seq`/`wal_min_seq`'s fallback path (issue
/// #375's follow-up extending the manifest to `wal_min_seq`) so that establishing — or
/// re-establishing — the manifest for either bound always leaves *both* bounds cached: a call to
/// the other function afterward gets the fast path "for free," without a second full scan.
///
/// Deliberately separate from `scan_max_seq_detailed`, which stays max-only and unchanged: that
/// one also backs `WalWriter::new`/`resync_global_seq`, hot paths that have never needed
/// `min_seq` and shouldn't pay `first_seq_in_file`'s extra per-file read to get it.
struct ScanWalBoundsDetail {
    max_seq: Option<u64>,
    max_seq_file: Option<String>,
    max_seq_file_size: u64,
    /// The literal lowest seq found across all files, or `None` if no file has a parseable line.
    min_seq: Option<u64>,
    /// File name of the file that held `min_seq`, so the manifest can cache it too.
    min_seq_file: Option<String>,
    /// Byte size of `min_seq_file` at the moment its first line was read (see the race-window
    /// note on the loop below) — used only to detect a later shrink/truncation of that file, not
    /// to detect growth (see [`WalBoundsManifest::min_seq_file_size`]).
    min_seq_file_size: u64,
    file_set_fingerprint: u64,
}

/// Reads all `.jsonl` files in `wal_dir` once and returns both the highest and lowest seq
/// present, along with which files held them — the shared full-scan fallback for both
/// `wal_max_seq` and `wal_min_seq`. See [`ScanWalBoundsDetail`].
fn scan_wal_bounds_detailed(wal_dir: &Path) -> Result<ScanWalBoundsDetail, Error> {
    let files: Vec<PathBuf> = fs::read_dir(wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    let file_set_fingerprint = jsonl_file_set_fingerprint(&files);

    let mut max_seq: Option<u64> = None;
    let mut max_seq_file: Option<String> = None;
    let mut max_seq_file_size: u64 = 0;
    let mut min_seq: Option<u64> = None;
    let mut min_seq_file: Option<String> = None;
    let mut min_seq_file_size: u64 = 0;
    for path in &files {
        if let Some(seq) = read_last_seq(path)? {
            if max_seq.is_none_or(|m| seq > m) {
                // Stat immediately after the read that produced this seq, not after the loop
                // finishes visiting every file: at directory scale, a full scan can take long
                // enough for a live writer to append further to this same file before the loop
                // ends, which would make a post-loop stat describe more bytes than the seq value
                // actually reflects — silently poisoning the fast path's later "unchanged size
                // means unchanged seq" assumption with a size that was already stale relative to
                // `seq` the moment it was recorded.
                max_seq = Some(seq);
                max_seq_file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                max_seq_file = path.file_name().map(|n| n.to_string_lossy().into_owned());
            }
        }
        if let Some(seq) = first_seq_in_file(path) {
            if min_seq.is_none_or(|m| seq < m) {
                min_seq = Some(seq);
                min_seq_file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                min_seq_file = path.file_name().map(|n| n.to_string_lossy().into_owned());
            }
        }
    }

    Ok(ScanWalBoundsDetail {
        max_seq,
        max_seq_file,
        max_seq_file_size,
        min_seq,
        min_seq_file,
        min_seq_file_size,
        file_set_fingerprint,
    })
}

/// File name (not full path — the manifest lives alongside the WAL files it describes) of the
/// on-disk cache `wal_max_seq` maintains so repeated calls against an unchanged WAL directory
/// don't have to reopen every `.jsonl` file. See issue #375 / ADR-0375.
const WAL_BOUNDS_MANIFEST_FILE: &str = ".wal-bounds.json";

/// Cached WAL seq bounds for a directory, persisted next to the WAL files it describes. The
/// `.json` extension keeps it invisible to every existing non-recursive `.jsonl` scan (FR-003):
/// `replay.rs`'s file lister, `count_jsonl_files`, and `scan_max_seq_detailed` all filter on
/// exact extension `"jsonl"`.
///
/// This is a read-side optimization layered over `scan_max_seq_detailed`, never a new source of
/// truth (FR-004): it is rebuilt from a full scan whenever it is absent, its
/// `file_set_fingerprint` no longer matches the directory, or the file it names has been removed
/// or is unparseable.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalBoundsManifest {
    /// Fingerprint of the `.jsonl` file *name set* present at manifest-write time (see
    /// [`jsonl_file_set_fingerprint`]). A live-writer file mid-append doesn't change this; any
    /// file being added, removed, or renamed does — so this is the cheap (one `read_dir`, no
    /// file opens) signal that directory membership may have changed since the manifest was
    /// written. A plain file *count* is deliberately not used here: it would miss a same-count
    /// swap (e.g. an external prune paired with a new rotation) that changes which file holds
    /// the true max seq without changing how many files are present.
    file_set_fingerprint: u64,
    /// File name of the `.jsonl` file that held `max_seq`. `None` iff `max_seq` is `None` (empty
    /// WAL directory).
    max_seq_file: Option<String>,
    /// Byte size of `max_seq_file` at manifest-write time. The only file in a WAL directory that
    /// can grow without changing the file name set is the one a live writer still has open — so
    /// an unchanged size, combined with an unchanged `file_set_fingerprint`, is sufficient to
    /// trust the cached `max_seq` without reopening anything.
    max_seq_file_size: u64,
    /// The literal highest seq present as of manifest-write time.
    max_seq: Option<u64>,
    /// File name of the `.jsonl` file that held `min_seq` at manifest-write time. `None` iff
    /// `min_seq` is `None`.
    ///
    /// A WAL file's first line is fixed the moment it's written, and a file only ever *grows* by
    /// appending to its end — ordinary growth can never change that first line. So unlike
    /// `max_seq_file`, growth in `min_seq_file` never needs a re-read: `wal_min_seq_fast_path`
    /// trusts the cached `min_seq` on an unchanged-or-larger size without reopening the file.
    min_seq_file: Option<String>,
    /// Byte size of `min_seq_file` at manifest-write time. Ordinary growth (the only thing that
    /// can legitimately happen to an already-scanned file) only ever increases this, so the fast
    /// path never re-reads on growth — but a *smaller* current size than recorded here means the
    /// file was truncated or replaced since, which can change its first line, so that case must
    /// force a full-scan fallback rather than keep trusting the cached `min_seq`.
    min_seq_file_size: u64,
    /// The literal lowest seq present as of manifest-write time.
    min_seq: Option<u64>,
}

fn wal_bounds_manifest_path(wal_dir: &Path) -> PathBuf {
    wal_dir.join(WAL_BOUNDS_MANIFEST_FILE)
}

/// Reads the manifest. Returns `None` if it is missing, corrupt, or unparseable — callers treat
/// that identically to "not established yet" and fall back to a full scan (FR-004).
fn read_wal_bounds_manifest(wal_dir: &Path) -> Option<WalBoundsManifest> {
    let text = fs::read_to_string(wal_bounds_manifest_path(wal_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Best-effort atomic write (tmp file + rename); failures are silently ignored. A manifest write
/// failure must never be fatal to the read it's caching (FR-004) — worst case, the next call pays
/// the full-scan cost again. The tmp file uses a UUID suffix (not a single fixed name, unlike
/// `ontology_sidecar`'s startup-time writer) because `wal_max_seq` can be called concurrently by
/// multiple async tasks in one process; a fixed tmp name would let concurrent writers corrupt
/// each other's in-flight write. Because each write picks a fresh UUID, a crash between
/// `fs::write` and `fs::rename` orphans that one tmp file forever — no future write ever reuses
/// its name to overwrite it. `sweep_stale_manifest_tmp_files` bounds that: a full-scan-fallback
/// write opportunistically clears out any `.tmp` sibling old enough to only be crash debris.
///
/// `sweep` is `true` only from the full-scan fallback path (`wal_max_seq`/`wal_min_seq`), which
/// is already paying a whole-directory `read_dir` cost. The growth-reconciliation write inside
/// `wal_max_seq_fast_path` runs on every call during active ingestion and passes `false` — that
/// path must not add a second `read_dir` per call on top of the one already paid for the
/// fingerprint check, or it would double the steady-state directory-listing cost this issue
/// exists to bound. Crash debris doesn't accumulate faster than one file per crash, so skipping
/// the sweep on the far more frequent growth-reconciliation write doesn't let it grow unbounded —
/// the next full-scan fallback (e.g. after any rotation) sweeps it.
fn write_wal_bounds_manifest(wal_dir: &Path, manifest: &WalBoundsManifest, sweep: bool) {
    if sweep {
        sweep_stale_manifest_tmp_files(wal_dir);
    }

    let Ok(json) = serde_json::to_string(manifest) else {
        return;
    };
    let tmp_path = wal_dir.join(format!(".wal-bounds.{}.tmp", Uuid::new_v4().as_simple()));
    if fs::write(&tmp_path, json).is_err() {
        let _ = fs::remove_file(&tmp_path);
        return;
    }
    if fs::rename(&tmp_path, wal_bounds_manifest_path(wal_dir)).is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
}

/// Age beyond which a leftover `.wal-bounds.*.tmp` file is assumed to be crash debris rather than
/// an in-flight concurrent write — writing and renaming a few-hundred-byte JSON file never
/// legitimately takes this long.
const STALE_MANIFEST_TMP_AGE: std::time::Duration = std::time::Duration::from_secs(300);

/// Removes `.wal-bounds.*.tmp` files older than [`STALE_MANIFEST_TMP_AGE`] — debris from a prior
/// `write_wal_bounds_manifest` call that crashed between `fs::write` and `fs::rename`, which a
/// UUID-suffixed tmp name (needed for concurrency safety, see above) means no later write will
/// ever land on and clean up by overwriting. Best-effort: any I/O error here is silently ignored,
/// same as the write it's paired with (FR-004 — never fatal to the read this caches).
fn sweep_stale_manifest_tmp_files(wal_dir: &Path) {
    let Ok(entries) = fs::read_dir(wal_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(".wal-bounds.") || !name.ends_with(".tmp") {
            continue;
        }
        let is_stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| {
                modified
                    .elapsed()
                    .is_ok_and(|age| age > STALE_MANIFEST_TMP_AGE)
            });
        if is_stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Returns the highest WAL `seq` actually present across `wal_dir` (issue #353), or `None` if
/// the WAL is empty — the same units as `applied_seq` (a literal seq value), unlike
/// `scan_max_seq`'s internal "next seq to assign" convention (`scan_max_seq() - 1` when
/// non-empty). This is what makes the `applied_seq == wal_max_seq` "caught up" check in
/// `knowledge_status` well-defined.
///
/// In the common case (a directory whose manifest is already established and hasn't been
/// invalidated by an out-of-band write since) this returns without opening any `.jsonl` file —
/// see [`WalBoundsManifest`] / ADR-0375. Any miss, mismatch, or corruption falls back to
/// `scan_wal_bounds_detailed`'s full scan, which the manifest is rebuilt from — so this always
/// returns the true value, never a filename-sorted or otherwise stale approximation, even at the
/// ~43,820-file production scale ADR-0026 documents.
///
/// The fallback scan computes `wal_min_seq`'s bound too (not just this function's own), so a
/// `wal_min_seq` call against the same directory afterward gets its fast path "for free" without
/// paying for a second full scan — see `scan_wal_bounds_detailed`.
pub fn wal_max_seq(wal_dir: &Path) -> Result<Option<u64>, Error> {
    if let Some(fast) = wal_max_seq_fast_path(wal_dir) {
        return Ok(fast);
    }

    let detailed = scan_wal_bounds_detailed(wal_dir)?;
    write_wal_bounds_manifest(
        wal_dir,
        &WalBoundsManifest {
            file_set_fingerprint: detailed.file_set_fingerprint,
            max_seq_file: detailed.max_seq_file.clone(),
            max_seq_file_size: detailed.max_seq_file_size,
            max_seq: detailed.max_seq,
            min_seq_file: detailed.min_seq_file.clone(),
            min_seq_file_size: detailed.min_seq_file_size,
            min_seq: detailed.min_seq,
        },
        true,
    );
    Ok(detailed.max_seq)
}

/// Whether `name` is safe to join directly onto `wal_dir` as `manifest.max_seq_file`: a single
/// path component with a `.jsonl` extension, no separators or `..`. The manifest is untrusted —
/// it tolerates being hand-edited, corrupted, or produced by a foreign writer (see
/// `read_wal_bounds_manifest`) — so a crafted or corrupted `max_seq_file` value (e.g. containing
/// `../`) must not be allowed to make `wal_dir.join(file_name)` resolve outside `wal_dir`.
fn is_safe_wal_file_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let path = Path::new(name);
    // Require the entire path to be exactly one `Normal` component. A plain separator/`..`
    // check is not enough on Windows: a value like `C:evil.jsonl` contains no `/` or `\` and
    // has a `.jsonl` extension, but carries a drive prefix with no root — `PathBuf::join`
    // treats a prefix-without-root component as replacing the base path outright rather than
    // appending to it, which would let a crafted/corrupted manifest escape `wal_dir` entirely.
    // Walking `components()` and requiring exactly one `Normal` rejects prefixes, roots, and
    // `..`/`.` uniformly on every platform.
    let mut components = path.components();
    let Some(std::path::Component::Normal(only)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }
    only.to_str().is_some_and(|s| s == name)
        && Path::new(name).extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// Attempts to answer `wal_max_seq` from the manifest alone. Returns `None` (not `Some(None)`)
/// on any miss, mismatch, or corruption — that's the "give up, caller should fall back to a full
/// scan" signal, distinct from `Some(None)`, which means "the fast path succeeded and the true
/// answer is an empty WAL directory."
fn wal_max_seq_fast_path(wal_dir: &Path) -> Option<Option<u64>> {
    let manifest = read_wal_bounds_manifest(wal_dir)?;
    // A directory that can't even be listed must fall back, not be treated as empty — otherwise
    // a `read_dir` error could spuriously "match" an empty-WAL manifest's fingerprint.
    let current_fingerprint = current_jsonl_file_set_fingerprint(wal_dir)?;
    if current_fingerprint != manifest.file_set_fingerprint {
        return None;
    }

    match (&manifest.max_seq_file, manifest.max_seq) {
        (None, None) => Some(None),
        (Some(file_name), Some(cached_seq)) => {
            if !is_safe_wal_file_name(file_name) {
                return None;
            }
            let file_path = wal_dir.join(file_name);
            let current_size = fs::metadata(&file_path).ok()?.len();
            if current_size == manifest.max_seq_file_size {
                return Some(Some(cached_seq));
            }
            // Same file name set, different size: the only file that can grow without changing
            // the name set is the one a live writer still has open, so re-read just that file's
            // tail rather than rescanning the whole directory. A live writer only ever appends,
            // so a re-read reporting a seq *lower* than what's cached means something other than
            // ordinary growth happened (e.g. truncation) — safer to fall back to a full scan than
            // to trust either value.
            let new_seq = match read_last_seq(&file_path) {
                Ok(Some(seq)) if seq >= cached_seq => seq,
                _ => return None,
            };
            write_wal_bounds_manifest(
                wal_dir,
                &WalBoundsManifest {
                    file_set_fingerprint: current_fingerprint,
                    max_seq_file: Some(file_name.clone()),
                    max_seq_file_size: current_size,
                    max_seq: Some(new_seq),
                    // Pass the existing min-seq fields through unchanged: the fingerprint match
                    // above already guarantees the file name set hasn't changed since they were
                    // cached, and growth in the max-seq file (even if it's the same file as the
                    // min-seq one) never changes that file's *first* line, so the cached min bound
                    // is still valid regardless of what happened to the max bound here.
                    min_seq_file: manifest.min_seq_file.clone(),
                    min_seq_file_size: manifest.min_seq_file_size,
                    min_seq: manifest.min_seq,
                },
                false,
            );
            Some(Some(new_seq))
        }
        // Inconsistent manifest shape (shouldn't happen from our own writer, but a hand-edited
        // or foreign-format file could produce this) — fall back to a full scan.
        _ => None,
    }
}

/// Returns the lowest WAL `seq` currently present across `wal_dir` (issue #365), or `None` if
/// the WAL is empty — the symmetric counterpart to `wal_max_seq`, used by the checkpoint
/// reachability check (FR-009) to bound a checkpoint's seq against the WAL content actually on
/// disk. On a full scan, this visits every `.jsonl` file rather than trusting filename sort order
/// to reflect seq order — a checkpoint reachability check that trusted sort order could misreport
/// a checkpoint as unreachable if a file sorting later on disk happens to hold a lower seq (e.g.
/// clock skew across concurrent writers sharing a WAL directory, per FR-012). Unlike
/// `wal_max_seq`'s tail-read trick (needed because the *last* line of a file sits at an unknown
/// offset), each file's *first* line is already at offset 0, so `first_seq_in_file` reads forward
/// only as far as the first parseable line — the same bounded-per-file cost class the full scan
/// already pays at the ADR-0026 ~43,820-file scale.
///
/// In the common case (a directory whose manifest is already established and hasn't been
/// invalidated by an out-of-band write since) this returns without opening any `.jsonl` file —
/// the same [`WalBoundsManifest`] `wal_max_seq` maintains, extended with a `min_seq`/`min_seq_file`
/// pair (issue #375's follow-up, once `wal_min_seq` existed on `main` — see ADR-0375). Any miss,
/// mismatch, or corruption falls back to `scan_wal_bounds_detailed`'s full scan, which both bounds
/// are rebuilt from — so this always returns the true value, never a filename-sorted or otherwise
/// stale approximation.
pub fn wal_min_seq(wal_dir: &Path) -> Result<Option<u64>, Error> {
    if let Some(fast) = wal_min_seq_fast_path(wal_dir) {
        return Ok(fast);
    }

    // A missing/unreadable WAL directory reports "empty" rather than erroring, matching this
    // function's pre-existing behavior (unlike `wal_max_seq`, which propagates the error).
    let Ok(detailed) = scan_wal_bounds_detailed(wal_dir) else {
        return Ok(None);
    };
    write_wal_bounds_manifest(
        wal_dir,
        &WalBoundsManifest {
            file_set_fingerprint: detailed.file_set_fingerprint,
            max_seq_file: detailed.max_seq_file.clone(),
            max_seq_file_size: detailed.max_seq_file_size,
            max_seq: detailed.max_seq,
            min_seq_file: detailed.min_seq_file.clone(),
            min_seq_file_size: detailed.min_seq_file_size,
            min_seq: detailed.min_seq,
        },
        true,
    );
    Ok(detailed.min_seq)
}

/// Attempts to answer `wal_min_seq` from the manifest alone, mirroring `wal_max_seq_fast_path`.
/// Returns `None` on any miss/mismatch/corruption (caller falls back to a full scan);
/// `Some(None)`/`Some(Some(seq))` on a fast-path hit.
///
/// Unlike `wal_max_seq_fast_path`, no re-read-on-growth step is needed for the `(Some, Some)`
/// case: as [`WalBoundsManifest::min_seq_file`]'s doc comment explains, ordinary append growth
/// can never change a file's first line, so growth alone never invalidates the cached `min_seq`.
/// A *shrink* below the recorded [`WalBoundsManifest::min_seq_file_size`] is different — the file
/// was truncated or replaced since, which can change its first line — so that one case still
/// needs a stat (cheap: one syscall, not a re-read) and forces a fallback rather than trusting a
/// cached value the on-disk file may no longer match.
///
/// The `(None, None)` case inherits the same accepted limitation `wal_max_seq_fast_path`'s
/// carries: a file with no parseable line at manifest-write time that later gains one via append,
/// without any change to the file *name* set, isn't detected here. Closing that gap would need a
/// new signal beyond what this issue's manifest mechanism already reuses; out of scope for this
/// extension.
fn wal_min_seq_fast_path(wal_dir: &Path) -> Option<Option<u64>> {
    let manifest = read_wal_bounds_manifest(wal_dir)?;
    let current_fingerprint = current_jsonl_file_set_fingerprint(wal_dir)?;
    if current_fingerprint != manifest.file_set_fingerprint {
        return None;
    }

    match (&manifest.min_seq_file, manifest.min_seq) {
        (None, None) => Some(None),
        (Some(file_name), Some(cached_seq)) => {
            if !is_safe_wal_file_name(file_name) {
                return None;
            }
            let file_path = wal_dir.join(file_name);
            let current_size = fs::metadata(&file_path).ok()?.len();
            if current_size < manifest.min_seq_file_size {
                // The file is smaller than it was when its first line produced `cached_seq` —
                // truncation or an in-place rewrite, not ordinary append growth. The first line
                // may no longer be what was cached, so this can't be trusted without a full scan.
                return None;
            }
            Some(Some(cached_seq))
        }
        // Inconsistent manifest shape — fall back to a full scan.
        _ => None,
    }
}

/// Reads a WAL file and returns the `seq` value of its first line that parses successfully.
/// Returns `None` only if the file can't be opened, or no line in it parses before EOF.
///
/// Reads incrementally via `BufReader::lines()` rather than loading the whole file into memory —
/// this can run once per `.jsonl` file up front (e.g. `replay_opts`'s file-ordering pass), so a
/// full-file read would turn a large WAL directory into an apparent startup hang. In the common
/// case the first line parses immediately and only that one line is read; only a genuinely
/// corrupt prefix costs more.
///
/// Tolerates a corrupt/truncated/non-UTF-8 leading line by skipping it and continuing, mirroring
/// `read_last_seq`'s tolerance: `io::Lines` reports an `Err` only for the one line that failed
/// UTF-8 conversion, having already consumed those bytes, so the next line is unaffected and
/// scanning can continue.
pub(crate) fn first_seq_in_file(path: &Path) -> Option<u64> {
    #[cfg(test)]
    FIRST_SEQ_IN_FILE_CALLS.with(|c| c.set(c.get() + 1));

    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader) {
        let raw = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(wal_line) = serde_json::from_str::<WalLine>(trimmed) {
            return Some(wal_line.seq);
        }
    }
    None
}

/// Bytes read from the tail of a WAL file before falling back to a full read. Generous even
/// for a line carrying a large embedding vector; chosen so `scan_max_seq`/`wal_max_seq` stay
/// cheap to call per `knowledge_status` request at the ~43,820-file scale ADR-0026 documents,
/// without reading each file's entire contents just to find its last line.
const TAIL_READ_WINDOW: u64 = 256 * 1024;

/// Removes every [`VECTOR_PARAM_KEYS`] entry from `params` (issue #526, FR-001). A no-op when
/// `params` isn't a JSON object (e.g. `Value::Null`, recorded by `Conn::raw_query` for
/// non-parameterized DDL) or carries none of these keys.
fn strip_vector_params(params: serde_json::Value) -> serde_json::Value {
    match params {
        serde_json::Value::Object(mut map) => {
            for key in VECTOR_PARAM_KEYS {
                map.remove(*key);
            }
            serde_json::Value::Object(map)
        }
        other => other,
    }
}

/// Returns a copy of `s` with Cypher single-quoted string literals replaced by a single space.
/// Handles `\'` escape sequences inside literals.  Used by `log_mutation` to prevent DML
/// keywords that happen to appear inside stored string values from being misclassified as
/// mutation queries.
///
/// Limitation: Cypher line comments (`//`) and block comments (`/* … */`) are not stripped.
/// A keyword appearing only inside a comment will pass through and trigger a false-positive
/// mutation classification. In practice, neither graphiti nor liminis-graph emits commented
/// Cypher in WAL lines, so this gap is benign.
pub(crate) fn strip_quoted_literals(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            // Consume until the matching closing quote, skipping \X escape sequences.
            loop {
                match chars.next() {
                    None => break,
                    Some('\\') => {
                        chars.next(); // skip the escaped char
                    }
                    Some('\'') => break,
                    _ => {}
                }
            }
            result.push(' ');
        } else {
            result.push(ch);
        }
    }
    result
}

/// Whether `cypher` is index DDL (`CREATE_VECTOR_INDEX`, `CREATE INDEX`, `CREATE ART INDEX`,
/// `DROP INDEX`), which `looks_like_mutation` would otherwise misclassify as an
/// `Entity`-mutating write on its `CREATE`/`DROP` keywords. Checked ahead of
/// `looks_like_mutation` wherever the result decides whether to pay for `Entity`-related work
/// (WAL logging) — index DDL never touches `Entity` rows, so it isn't needed for it (higher
/// priority per AD-W7).
///
/// `"CREATE ART INDEX"` (issue #221, `Entity.lookup_key`'s secondary index) is checked
/// separately from `"CREATE INDEX"`: the `ART` token sitting between `CREATE` and `INDEX` means
/// the plain substring check doesn't match it.
pub(crate) fn is_index_ddl(cypher: &str) -> bool {
    let upper = cypher.to_uppercase();
    upper.contains("CREATE_VECTOR_INDEX")
        || upper.contains("CREATE INDEX")
        || upper.contains("CREATE ART INDEX")
        || upper.contains("DROP INDEX")
}

/// Whether `cypher` looks like a write (as opposed to a read-only query), by scanning all
/// tokens outside single-quoted literals for mutation keywords. MATCH-prefixed writes (e.g.
/// `"MATCH (...) DETACH DELETE"` or `"MATCH (...) SET ..."`) don't start with the DML verb,
/// so a first-token check would miss them. Stripping quoted literals first prevents entity
/// names that happen to contain DML words from being misclassified as mutations. Parentheses
/// are also treated as token boundaries (in addition to whitespace) so a keyword directly
/// touching punctuation with no space — `"CREATE(:Entity"`, `"MATCH (n)SET"` — still tokenizes
/// as a standalone word instead of being swallowed into one run that never matches exactly.
/// This can only ever turn a prior false negative into a (correct) match, never the reverse,
/// since it splits existing tokens further rather than merging any together.
///
/// `log_mutation`'s inline WAL-logging check. Prior to issue #221, `handle_query_cypher` also
/// reused this heuristic to decide whether a raw-Cypher call through the `cypher` MCP scope
/// needed a follow-up `NameIndex` rebuild; that call site is gone now that `Entity.lookup_key`'s
/// secondary ART index is maintained by lbug itself and there is nothing left to rebuild.
/// Callers that care about index DDL specifically (see `is_index_ddl`) should check that first,
/// since this function alone would classify `CREATE INDEX ...` as a mutation.
pub(crate) fn looks_like_mutation(cypher: &str) -> bool {
    let upper = cypher.to_uppercase();
    let stripped = strip_quoted_literals(&upper);
    let spaced: String = stripped
        .chars()
        .flat_map(|c| {
            if c == '(' || c == ')' {
                vec![' ', c, ' ']
            } else {
                vec![c]
            }
        })
        .collect();
    spaced.split_whitespace().any(|t| {
        matches!(
            t,
            "CREATE" | "MERGE" | "SET" | "DELETE" | "DETACH" | "DROP" | "REMOVE"
        )
    })
}

/// Returns the `seq` from the last parseable non-empty line in the file, or `None`.
///
/// Reads only a bounded tail window (`TAIL_READ_WINDOW`) first — the common case, one complete
/// trailing line well within the window — and falls back to a full-file read only when the
/// window yields no parseable `seq` (e.g. a single line larger than the window, such as one
/// carrying an unusually large embedding vector). This preserves the existing "tolerates
/// truncated final lines" guarantee: truncation can only affect the *end* of the file, and both
/// paths always read through to EOF.
fn read_last_seq(path: &Path) -> Result<Option<u64>, Error> {
    #[cfg(test)]
    READ_LAST_SEQ_CALLS.with(|c| c.set(c.get() + 1));

    let file_len = fs::metadata(path)?.len();
    if file_len > TAIL_READ_WINDOW {
        if let Some(seq) = read_last_seq_in_range(path, file_len - TAIL_READ_WINDOW)? {
            return Ok(Some(seq));
        }
    }
    read_last_seq_in_range(path, 0)
}

/// Reads `path` from byte offset `start` through EOF and returns the `seq` of the last
/// parseable non-empty line. When `start > 0`, the first (possibly partial) line in the window
/// is discarded, since `start` may land mid-line.
fn read_last_seq_in_range(path: &Path, start: u64) -> Result<Option<u64>, Error> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = fs::File::open(path)?;
    let mut buf = Vec::new();
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);

    let mut lines = text.lines();
    if start > 0 {
        lines.next(); // drop the partial line the seek landed inside
    }

    for raw in lines.rev() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(seq) = val.get("seq").and_then(|v| v.as_u64()) {
                return Ok(Some(seq));
            }
        }
    }
    Ok(None)
}

// Counts calls to `read_last_seq` — the sole choke point that opens a `.jsonl` file's content —
// so tests can assert "this call did not reopen any WAL file" directly (FR-001/SC-001), rather
// than relying on wall-clock timing. Thread-local (not a shared global) because the standard test
// harness runs each `#[test]` on its own thread and this crate's tests run with real parallelism
// (`RUST_TEST_THREADS=4`, see `.cargo/config.toml`) — a shared counter would be incremented by
// unrelated tests running concurrently and make these assertions flaky.
#[cfg(test)]
thread_local! {
    static READ_LAST_SEQ_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_read_last_seq_call_count() {
    READ_LAST_SEQ_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
fn read_last_seq_call_count() -> usize {
    READ_LAST_SEQ_CALLS.with(|c| c.get())
}

// Mirrors READ_LAST_SEQ_CALLS above, but for `first_seq_in_file` — the sole choke point that
// opens a `.jsonl` file's content for a min-seq read — so `wal_min_seq`'s fast path can be
// asserted against the same "did not reopen any file" contract `wal_max_seq`'s already is.
#[cfg(test)]
thread_local! {
    static FIRST_SEQ_IN_FILE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_first_seq_in_file_call_count() {
    FIRST_SEQ_IN_FILE_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
fn first_seq_in_file_call_count() -> usize {
    FIRST_SEQ_IN_FILE_CALLS.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── log_mutation vector-param stripping (issue #526, FR-001) ────────────────────────────

    /// Reads every `.jsonl` line back out of `dir`, across every file, in file-then-line order —
    /// enough for these tests, which each write exactly one line via a single `with_chunk` call.
    fn read_wal_lines(dir: &Path) -> Vec<WalLine> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        files
            .iter()
            .flat_map(|p| {
                fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str::<WalLine>(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn log_mutation_strips_vector_params_from_a_create() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: \
                     $name_embedding, summary: $summary, summary_embedding: \
                     $summary_embedding})",
                    serde_json::json!({
                        "uuid": "e1",
                        "name": "Alice",
                        "name_embedding": [1.0, 0.0, 0.0, 0.0],
                        "summary": "Alice summary",
                        "summary_embedding": [0.0, 1.0, 0.0, 0.0],
                    }),
                    "test",
                )
            })
            .unwrap();

        let lines = read_wal_lines(tmp.path());
        let params = &lines[0].params;
        assert!(params.get("name_embedding").is_none());
        assert!(params.get("summary_embedding").is_none());
        assert_eq!(params.get("uuid").unwrap(), "e1");
        assert_eq!(params.get("name").unwrap(), "Alice");
        assert_eq!(params.get("summary").unwrap(), "Alice summary");
    }

    #[test]
    fn log_mutation_leaves_non_vector_params_and_mutations_unaffected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "MERGE (n:Entity {uuid: $uuid}) SET n.name = $name",
                    serde_json::json!({ "uuid": "e1", "name": "Alice" }),
                    "test",
                )
            })
            .unwrap();

        let lines = read_wal_lines(tmp.path());
        let params = &lines[0].params;
        assert_eq!(params.get("uuid").unwrap(), "e1");
        assert_eq!(params.get("name").unwrap(), "Alice");
    }

    #[test]
    fn log_mutation_strips_vector_params_from_a_dump_shaped_direct_call() {
        // dump.rs calls `WalWriter::log_mutation` directly (not through `wal_exec.rs`'s
        // `wal_params()` helper) — this test pins that the strip still applies via the one
        // choke point every write path shares, matching `dump.rs`'s MERGE-form re-materialization
        // templates.
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "MERGE (n:Episodic {uuid: $uuid}) SET n.content = $content, \
                     n.content_embedding = $content_embedding",
                    serde_json::json!({
                        "uuid": "ep1",
                        "content": "some text",
                        "content_embedding": [1.0, 2.0, 3.0, 4.0],
                    }),
                    "test",
                )
            })
            .unwrap();

        let lines = read_wal_lines(tmp.path());
        let params = &lines[0].params;
        assert!(params.get("content_embedding").is_none());
        assert_eq!(params.get("content").unwrap(), "some text");
    }

    #[test]
    fn log_mutation_strip_is_a_no_op_for_null_params() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "CREATE (:Entity {uuid: 'x'})",
                    serde_json::Value::Null,
                    "test",
                )
            })
            .unwrap();

        let lines = read_wal_lines(tmp.path());
        assert_eq!(lines[0].params, serde_json::Value::Null);
    }

    #[test]
    fn looks_like_mutation_detects_keywords_touching_parentheses() {
        // Regression for a punctuation-adjacency gap: split_whitespace alone treats
        // "CREATE(:Entity" and "N)SET" as single opaque tokens that never equal "CREATE"/"SET"
        // exactly, silently missing real mutations that happen to have no space before/after
        // a paren.
        assert!(looks_like_mutation("CREATE(:Entity {uuid: 'x'})"));
        assert!(looks_like_mutation("MATCH (n)SET n.x = 1"));
        assert!(looks_like_mutation("MATCH (n)DETACH DELETE n"));
    }

    #[test]
    fn looks_like_mutation_still_ignores_read_only_queries() {
        assert!(!looks_like_mutation("MATCH (n) RETURN n"));
        assert!(!looks_like_mutation("MATCH(n)RETURN n"));
    }

    fn write_wal_line(dir: &Path, file_name: &str, seq: u64) {
        let line = WalLine {
            seq,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "x" }),
        };
        fs::write(
            dir.join(file_name),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn resync_global_seq_is_monotonic_against_empty_wal_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        // Simulate mutations already emitted this process (in-memory global_seq advanced).
        writer.global_seq = 10;

        writer.resync_global_seq(None).unwrap();

        assert_eq!(writer.global_seq, 10, "resync must not lower global_seq");
    }

    #[test]
    fn resync_global_seq_picks_up_files_written_after_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let writer_state = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        assert_eq!(writer_state.global_seq, 0);
        let mut writer = writer_state;

        // WAL dir populated out-of-band after the writer was constructed.
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 41);

        writer.resync_global_seq(None).unwrap();

        assert_eq!(writer.global_seq, 42, "resync must pick up on-disk seqs");
    }

    #[test]
    fn resync_global_seq_uses_last_committed_seq_as_a_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();

        // No on-disk lines beyond what the replay itself committed; last_committed_seq alone
        // must be enough to raise the floor.
        writer.resync_global_seq(Some(99)).unwrap();

        assert_eq!(writer.global_seq, 100);
    }

    #[test]
    fn resync_global_seq_prefers_on_disk_scan_over_lower_last_committed_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();

        // A line that failed to replay still has a higher on-disk seq than the last
        // successfully committed line (FR-003 edge case).
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 200);

        writer.resync_global_seq(Some(5)).unwrap();

        assert_eq!(writer.global_seq, 201);
    }

    #[test]
    fn wal_max_seq_is_none_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), None);
    }

    #[test]
    fn wal_max_seq_reports_the_literal_highest_seq() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 41);

        // `scan_max_seq` (next-assignable) would return 42; `wal_max_seq` must report 41 — the
        // same units as `applied_seq`, so `applied_seq == wal_max_seq` can express "caught up".
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(41));
    }

    #[test]
    fn wal_min_seq_is_none_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), None);
    }

    #[test]
    fn wal_min_seq_is_none_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(wal_min_seq(&missing).unwrap(), None);
    }

    #[test]
    fn wal_min_seq_reports_the_literal_lowest_seq_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "seeded_0001.jsonl", 41);
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 10);

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(10));
    }

    /// Guards against trusting filename sort order for correctness: the lexicographically-first
    /// file here holds the *higher* seq, mirroring the clock-skew/multi-writer scenario the
    /// checkpoint reachability check (FR-009, issue #365) must not misreport. `wal_min_seq` must
    /// scan every file for the true minimum, the same way `scan_max_seq` does for the maximum.
    #[test]
    fn wal_min_seq_finds_the_true_minimum_even_when_the_first_sorted_file_is_not_lowest() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_first_sorted.jsonl", 100);
        write_wal_line(tmp.path(), "b_second_sorted.jsonl", 5);

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(5));
    }

    #[test]
    fn wal_min_seq_tolerates_a_corrupt_leading_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("corrupt_0000.jsonl"), "not json\n").unwrap();
        write_wal_line(tmp.path(), "seeded_0001.jsonl", 10);

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(10));
    }

    /// SC-009 (issue #365): the `.checkpoints/` subdirectory a WAL directory may contain must be
    /// invisible to every non-recursive `.jsonl`-extension-filtered scan in this file — it is
    /// neither counted by `count_jsonl_files` (`knowledge_status`'s `wal.file_count`) nor folded
    /// into `scan_max_seq`/`wal_max_seq`/`wal_min_seq` (`global_seq` allocation and checkpoint
    /// reachability), even though it holds JSON files of its own one level down.
    #[test]
    fn checkpoints_subdirectory_is_invisible_to_jsonl_scans() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 10);

        let checkpoint_dir = tmp.path().join(".checkpoints").join("pre-migration");
        fs::create_dir_all(&checkpoint_dir).unwrap();
        fs::write(
            checkpoint_dir.join("g1.create.json"),
            r#"{"name":"pre-migration","seq":10,"created_at_ms":0}"#,
        )
        .unwrap();
        // Also drop a same-named `.jsonl` sibling one level down, to prove non-recursion (not
        // just extension filtering) is what keeps it out of scope.
        fs::write(checkpoint_dir.join("decoy.jsonl"), "not a real WAL line\n").unwrap();

        assert_eq!(count_jsonl_files(tmp.path()), 1);
        assert_eq!(scan_max_seq(tmp.path()).unwrap(), 11);
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(10));
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(10));
    }

    /// A single WAL line larger than `TAIL_READ_WINDOW` (e.g. one carrying an unusually large
    /// embedding) must still be found via the full-read fallback.
    #[test]
    fn read_last_seq_falls_back_to_full_read_for_an_oversized_single_line() {
        let tmp = tempfile::tempdir().unwrap();
        let line = WalLine {
            seq: 7,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            // Padding comfortably exceeds TAIL_READ_WINDOW so the tail-read pass alone can't
            // find a complete line, forcing the full-read fallback path.
            params: serde_json::json!({ "uuid": "x".repeat((TAIL_READ_WINDOW as usize) + 1024) }),
        };
        fs::write(
            tmp.path().join("oversized_0000.jsonl"),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();

        assert_eq!(
            read_last_seq(&tmp.path().join("oversized_0000.jsonl")).unwrap(),
            Some(7)
        );
    }

    /// A file well beyond the tail window with many prior lines must still resolve to the last
    /// line's `seq`, exercising the tail-read path (not the full-read fallback) end to end.
    #[test]
    fn read_last_seq_tail_read_finds_last_line_in_large_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("large_0000.jsonl");
        let mut content = String::new();
        // Enough short lines to exceed TAIL_READ_WINDOW comfortably.
        for seq in 0..20_000u64 {
            let line = WalLine {
                seq,
                ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
                db: "default".to_string(),
                cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
                params: serde_json::json!({ "uuid": "x" }),
            };
            content.push_str(&serde_json::to_string(&line).unwrap());
            content.push('\n');
        }
        fs::write(&path, &content).unwrap();
        assert!(
            content.len() as u64 > TAIL_READ_WINDOW,
            "test file must exceed the tail-read window to exercise that path"
        );

        assert_eq!(read_last_seq(&path).unwrap(), Some(19_999));
    }

    /// A truncated final line (partial JSON, e.g. from a crash mid-write) must be skipped in
    /// favor of the last complete line before it — preserved by both the tail-read and
    /// full-read paths.
    #[test]
    fn read_last_seq_tolerates_truncated_final_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("truncated_0000.jsonl");
        write_wal_line(tmp.path(), "truncated_0000.jsonl", 3);
        // Append a truncated (invalid JSON) final line, as a crash mid-write would leave.
        let mut existing = fs::read_to_string(&path).unwrap();
        existing.push_str(r#"{"seq":4,"ts":"2026-08-05T00:00:00"#);
        fs::write(&path, existing).unwrap();

        assert_eq!(read_last_seq(&path).unwrap(), Some(3));
    }

    /// SC-001/SC-005: once the manifest is established, a second `wal_max_seq` call against an
    /// unchanged directory must not reopen a single `.jsonl` file — regardless of how many files
    /// are present.
    #[test]
    fn wal_max_seq_warm_cache_avoids_reopening_files_at_scale() {
        let tmp = tempfile::tempdir().unwrap();
        let n = 3_000u64;
        for i in 0..n {
            write_wal_line(tmp.path(), &format!("f_{:06}.jsonl", i), i);
        }

        // Cold call: establishes the manifest, still returns the correct value.
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(n - 1));

        reset_read_last_seq_call_count();
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(n - 1));
        assert_eq!(
            read_last_seq_call_count(),
            0,
            "warm-cache call must not reopen any .jsonl file"
        );
    }

    /// FR-008: a new file appearing between calls (rotation, or an external writer) changes the
    /// `.jsonl` file *name set*, which must force a reconciling full scan rather than serve a
    /// stale cached value.
    #[test]
    fn wal_max_seq_detects_new_file_via_fingerprint_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 5);
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(5)); // establishes the manifest

        // Simulates rotation or an external writer adding a file with a higher seq.
        write_wal_line(tmp.path(), "b_0000.jsonl", 9);

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(9));
    }

    /// FR-008: the file *name set* doesn't change when a live writer keeps appending to the file
    /// it already has open (no rotation yet) — that's detected via the tracked file's byte size
    /// instead, and reconciled with exactly one re-read of that file, not a full rescan.
    #[test]
    fn wal_max_seq_detects_growth_in_tracked_file_with_single_reread() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 5);
        write_wal_line(tmp.path(), "b_0000.jsonl", 1); // lower seq; never the tracked max file

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(5));

        // Append another line to the tracked max-seq file, simulating active ingestion that
        // hasn't rotated yet.
        let path = tmp.path().join("a_0000.jsonl");
        let mut existing = fs::read_to_string(&path).unwrap();
        let extra_line = WalLine {
            seq: 42,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "y" }),
        };
        existing.push_str(&serde_json::to_string(&extra_line).unwrap());
        existing.push('\n');
        fs::write(&path, existing).unwrap();

        reset_read_last_seq_call_count();
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(42));
        assert_eq!(
            read_last_seq_call_count(),
            1,
            "growth in the tracked max-seq file must cost exactly one re-read, not a full rescan"
        );
    }

    /// FR-004: a truncated/corrupt manifest must not be trusted — it's treated the same as
    /// "absent," falls back to a full scan, and self-heals (a fresh, valid manifest is written)
    /// rather than permanently degrading to full-scan-every-time.
    #[test]
    fn wal_max_seq_recovers_from_corrupt_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 7);
        fs::write(tmp.path().join(".wal-bounds.json"), b"{not valid json").unwrap();

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(7));

        // Self-healed: the manifest written by the fallback above should now serve the
        // warm-cache path without reopening any file.
        reset_read_last_seq_call_count();
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(7));
        assert_eq!(read_last_seq_call_count(), 0);
    }

    /// Edge case: an empty WAL directory must keep reporting `None` on both the first (cold) and
    /// a subsequent (warm-cache) call.
    #[test]
    fn wal_max_seq_empty_dir_stays_none_warm_and_cold() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), None);
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), None);
    }

    /// SC-004: a directory with no manifest at all — predating this feature, or written entirely
    /// by the Python producer, which never creates one — must still return the correct value on
    /// first access.
    #[test]
    fn wal_max_seq_first_access_with_no_manifest_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "py_0000.jsonl", 3);
        write_wal_line(tmp.path(), "py_0001.jsonl", 12);

        assert!(!tmp.path().join(".wal-bounds.json").exists());
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(12));
    }

    /// FR-003: the manifest file itself must stay invisible to the existing non-recursive
    /// `.jsonl` scans — `count_jsonl_files` here stands in for the shared extension-filter
    /// discipline `replay.rs`'s file lister and `scan_max_seq_detailed` also use.
    #[test]
    fn wal_bounds_manifest_is_invisible_to_jsonl_file_count() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 1);
        wal_max_seq(tmp.path()).unwrap(); // establishes the manifest
        assert!(tmp.path().join(".wal-bounds.json").exists());

        assert_eq!(count_jsonl_files(tmp.path()), 1);
    }

    /// FR-008: a same-count swap — one file removed, a different one added, leaving the total
    /// `.jsonl` count unchanged and never touching the tracked max-seq file — must still be
    /// detected and reconciled. A staleness check based on file *count* alone would miss this
    /// (2 files before, 2 files after) and silently serve the stale cached max.
    #[test]
    fn wal_max_seq_detects_same_count_file_swap() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 5); // tracked max
        write_wal_line(tmp.path(), "b_0000.jsonl", 3);
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(5)); // establishes manifest, count=2

        // Remove the non-tracked file, add a different one with a HIGHER seq than tracked max.
        // Net file count stays at 2, and the tracked file "a_0000.jsonl" is untouched.
        fs::remove_file(tmp.path().join("b_0000.jsonl")).unwrap();
        write_wal_line(tmp.path(), "c_0000.jsonl", 10);

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(10));
    }

    /// The manifest is untrusted input (it tolerates hand-editing/corruption/foreign writers) —
    /// a crafted `max_seq_file` value must not be allowed to make `wal_dir.join(file_name)`
    /// escape the WAL directory. A traversal value must be rejected, forcing a fallback to a
    /// full scan that returns the true (unrelated) on-disk value instead.
    #[test]
    fn wal_max_seq_rejects_path_traversal_in_manifest_max_seq_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 5);

        let real_files = vec![tmp.path().join("a_0000.jsonl")];
        let malicious = WalBoundsManifest {
            file_set_fingerprint: jsonl_file_set_fingerprint(&real_files),
            max_seq_file: Some("../evil.jsonl".to_string()),
            max_seq_file_size: 0,
            max_seq: Some(999_999),
            min_seq_file: None,
            min_seq_file_size: 0,
            min_seq: None,
        };
        fs::write(
            tmp.path().join(".wal-bounds.json"),
            serde_json::to_string(&malicious).unwrap(),
        )
        .unwrap();

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(5));
    }

    /// `is_safe_wal_file_name` must reject any value whose `components()` yield more than a
    /// single `Normal` component — separators, `..`/`.`, and (platform-dependently) prefixes/
    /// roots all fail this uniformly. Cross-platform subset: valid on every host OS this crate
    /// builds for.
    #[test]
    fn is_safe_wal_file_name_rejects_traversal_and_separators() {
        assert!(!is_safe_wal_file_name("../evil.jsonl"));
        assert!(!is_safe_wal_file_name("a/b.jsonl"));
        assert!(!is_safe_wal_file_name(".."));
        assert!(!is_safe_wal_file_name("."));
        assert!(!is_safe_wal_file_name(""));
        assert!(is_safe_wal_file_name("a_0000.jsonl"));
    }

    /// On Windows specifically, a drive-prefix-without-root value like `C:evil.jsonl` contains
    /// no `/` or `\` and ends in `.jsonl`, so a check that only scans for separator characters
    /// would wrongly accept it — but `Path::join` treats a prefix-without-root component as
    /// replacing the base path outright, an escape from `wal_dir`. This is a no-op string
    /// (`"C:evil.jsonl"` is just an ordinary, safe filename) on Unix, so the assertion is
    /// meaningful only under `cfg(windows)`.
    #[cfg(windows)]
    #[test]
    fn is_safe_wal_file_name_rejects_windows_drive_prefix() {
        assert!(!is_safe_wal_file_name("C:evil.jsonl"));
        assert!(!is_safe_wal_file_name(r"C:\evil.jsonl"));
    }

    /// A live writer only ever appends, so a re-read of the tracked max-seq file reporting a
    /// *lower* seq than cached (simulating truncation/corruption, not ordinary growth) must not
    /// be trusted directly — it must force a full-scan fallback rather than serve the suspect
    /// re-read value from the single-file fast path.
    #[test]
    fn wal_max_seq_falls_back_when_tracked_file_reread_seq_decreases() {
        let tmp = tempfile::tempdir().unwrap();
        let a_path = tmp.path().join("a_0000.jsonl");
        write_wal_line(tmp.path(), "a_0000.jsonl", 1);
        let first_line = fs::read_to_string(&a_path).unwrap();

        let second_line = WalLine {
            seq: 5,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "y" }),
        };
        fs::write(
            &a_path,
            format!(
                "{}{}\n",
                first_line,
                serde_json::to_string(&second_line).unwrap()
            ),
        )
        .unwrap();
        write_wal_line(tmp.path(), "b_0000.jsonl", 2);

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(5)); // establishes manifest, tracked = a_0000

        // Truncate a_0000.jsonl back to just its first line — data loss, not ordinary append
        // growth. The tracked file's re-read would report seq=1, lower than the cached seq=5.
        fs::write(&a_path, &first_line).unwrap();

        reset_read_last_seq_call_count();
        let result = wal_max_seq(tmp.path()).unwrap();
        assert_eq!(
            result,
            Some(2),
            "true max after truncation is b_0000's seq=2"
        );
        assert!(
            read_last_seq_call_count() >= 2,
            "a seq decrease on the tracked file must trigger a full-scan fallback (visiting every \
             file), not be trusted directly from a single re-read"
        );
    }

    /// SC-001/SC-005 extended to `wal_min_seq` (issue #375 follow-up): once the manifest is
    /// established, a second `wal_min_seq` call against an unchanged directory must not reopen a
    /// single `.jsonl` file, regardless of how many files are present.
    #[test]
    fn wal_min_seq_warm_cache_avoids_reopening_files_at_scale() {
        let tmp = tempfile::tempdir().unwrap();
        let n = 3_000u64;
        for i in 0..n {
            write_wal_line(tmp.path(), &format!("f_{:06}.jsonl", i), i);
        }

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(0));

        reset_first_seq_in_file_call_count();
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(0));
        assert_eq!(
            first_seq_in_file_call_count(),
            0,
            "warm-cache call must not reopen any .jsonl file"
        );
    }

    /// A `wal_max_seq` fallback scan establishes `min_seq` too (via `scan_wal_bounds_detailed`),
    /// so a subsequent `wal_min_seq` call against the same directory hits its fast path
    /// immediately without ever doing its own full scan — and vice versa. This is the whole point
    /// of extending the manifest to both bounds rather than maintaining two independent caches.
    #[test]
    fn establishing_one_bound_benefits_the_other() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 5);
        write_wal_line(tmp.path(), "b_0000.jsonl", 1);

        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(5)); // cold: establishes both bounds

        reset_first_seq_in_file_call_count();
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(1));
        assert_eq!(
            first_seq_in_file_call_count(),
            0,
            "wal_min_seq must be able to serve from the manifest wal_max_seq's fallback wrote, \
             without any file reads of its own"
        );

        // And the reverse direction.
        let tmp2 = tempfile::tempdir().unwrap();
        write_wal_line(tmp2.path(), "a_0000.jsonl", 5);
        write_wal_line(tmp2.path(), "b_0000.jsonl", 1);

        assert_eq!(wal_min_seq(tmp2.path()).unwrap(), Some(1)); // cold: establishes both bounds

        reset_read_last_seq_call_count();
        assert_eq!(wal_max_seq(tmp2.path()).unwrap(), Some(5));
        assert_eq!(
            read_last_seq_call_count(),
            0,
            "wal_max_seq must be able to serve from the manifest wal_min_seq's fallback wrote, \
             without any file reads of its own"
        );
    }

    /// FR-008 extended to `wal_min_seq`: a file being added or removed changes the `.jsonl` file
    /// *name* set, which the shared `file_set_fingerprint` check already forces a reconciling
    /// full scan on for both bounds — including when the change affects the minimum specifically
    /// (e.g. retention trimming the oldest file away).
    #[test]
    fn wal_min_seq_detects_file_removal_via_fingerprint_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_oldest.jsonl", 1);
        write_wal_line(tmp.path(), "b_0000.jsonl", 5);
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(1)); // establishes manifest

        // Simulates retention removing the oldest WAL file.
        fs::remove_file(tmp.path().join("a_oldest.jsonl")).unwrap();

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(5));
    }

    /// FR-004 extended to `wal_min_seq`: a truncated/corrupt manifest is treated as absent, falls
    /// back to a full scan, and self-heals rather than degrading permanently.
    #[test]
    fn wal_min_seq_recovers_from_corrupt_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 7);
        fs::write(tmp.path().join(".wal-bounds.json"), b"{not valid json").unwrap();

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(7));

        reset_first_seq_in_file_call_count();
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(7));
        assert_eq!(first_seq_in_file_call_count(), 0);
    }

    /// Edge case extended to `wal_min_seq`: an empty WAL directory must keep reporting `None` on
    /// both the first (cold) and a subsequent (warm-cache) call.
    #[test]
    fn wal_min_seq_empty_dir_stays_none_warm_and_cold() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), None);
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), None);
    }

    /// SC-004 extended to `wal_min_seq`: a directory with no manifest at all — predating this
    /// feature, or written entirely by the Python producer — must still return the correct value
    /// on first access.
    #[test]
    fn wal_min_seq_first_access_with_no_manifest_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "py_0000.jsonl", 3);
        write_wal_line(tmp.path(), "py_0001.jsonl", 12);

        assert!(!tmp.path().join(".wal-bounds.json").exists());
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(3));
    }

    /// The manifest is untrusted input, mirroring
    /// `wal_max_seq_rejects_path_traversal_in_manifest_max_seq_file` — a crafted `min_seq_file`
    /// value must not be allowed to make `wal_dir.join(file_name)` escape the WAL directory.
    #[test]
    fn wal_min_seq_rejects_path_traversal_in_manifest_min_seq_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "a_0000.jsonl", 5);

        let real_files = vec![tmp.path().join("a_0000.jsonl")];
        let malicious = WalBoundsManifest {
            file_set_fingerprint: jsonl_file_set_fingerprint(&real_files),
            max_seq_file: Some("a_0000.jsonl".to_string()),
            max_seq_file_size: fs::metadata(tmp.path().join("a_0000.jsonl")).unwrap().len(),
            max_seq: Some(5),
            min_seq_file: Some("../evil.jsonl".to_string()),
            min_seq_file_size: 0,
            min_seq: Some(0),
        };
        fs::write(
            tmp.path().join(".wal-bounds.json"),
            serde_json::to_string(&malicious).unwrap(),
        )
        .unwrap();

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(5));
    }

    /// A cached `min_seq` must not be trusted once its file has shrunk below the size recorded
    /// when that seq was derived: ordinary append growth can never change a file's first line,
    /// but a smaller size means the file was truncated or replaced, which can. This mirrors
    /// `wal_max_seq_falls_back_when_tracked_file_reread_seq_decreases`'s defense for the max-seq
    /// side, closing the analogous gap on the min-seq side (the fast path previously had no size
    /// check at all for `min_seq_file`).
    #[test]
    fn wal_min_seq_falls_back_when_tracked_file_shrinks() {
        let tmp = tempfile::tempdir().unwrap();
        let a_path = tmp.path().join("a_0000.jsonl");
        // Two lines so the file can shrink (losing the second) while still being non-empty.
        let first_line = WalLine {
            seq: 1,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "x" }),
        };
        let second_line = WalLine {
            seq: 2,
            ts: "2026-08-05T00:00:01.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "y" }),
        };
        let first_line_text = format!("{}\n", serde_json::to_string(&first_line).unwrap());
        fs::write(
            &a_path,
            format!(
                "{}{}\n",
                first_line_text,
                serde_json::to_string(&second_line).unwrap()
            ),
        )
        .unwrap();
        write_wal_line(tmp.path(), "b_0000.jsonl", 9); // higher seq, never the tracked min file

        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(1)); // establishes manifest, tracked = a_0000

        // Truncate a_0000.jsonl to nothing, then rewrite it with a different first line — data
        // loss/replacement, not ordinary append growth. Net size ends up smaller than recorded.
        let replacement = WalLine {
            seq: 42,
            ts: "2026-08-05T00:00:02.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "z" }),
        };
        fs::write(
            &a_path,
            format!("{}\n", serde_json::to_string(&replacement).unwrap()),
        )
        .unwrap();
        assert!(
            fs::metadata(&a_path).unwrap().len() < first_line_text.len() as u64 * 2,
            "test setup invariant: the replacement file must be smaller than the original \
             two-line file for this to exercise the shrink-detection path"
        );

        reset_first_seq_in_file_call_count();
        let result = wal_min_seq(tmp.path()).unwrap();
        assert_eq!(
            result,
            Some(9),
            "true min after replacement is b_0000's seq=9"
        );
        assert!(
            first_seq_in_file_call_count() >= 2,
            "a shrink on the tracked min-seq file must trigger a full-scan fallback (visiting \
             every file), not silently keep serving the stale cached min"
        );
    }

    #[test]
    fn new_writer_on_empty_dir_mints_a_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        let g = writer
            .generation()
            .expect("fresh stream must mint a generation");
        assert_eq!(
            crate::wal_generation::read_generation(tmp.path()).as_deref(),
            Some(g)
        );
    }

    #[test]
    fn reopening_writer_reads_the_same_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let writer1 = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        let g1 = writer1.generation().unwrap().to_string();
        drop(writer1);

        let writer2 = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        assert_eq!(writer2.generation(), Some(g1.as_str()));
    }

    #[test]
    fn writer_over_pre_existing_content_with_no_generation_stays_none() {
        let tmp = tempfile::tempdir().unwrap();
        // Simulate a pre-#387 populated stream: content on disk, no generation sidecar.
        write_wal_line(tmp.path(), "0000.jsonl", 0);

        let writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        assert_eq!(
            writer.generation(),
            None,
            "a legacy populated stream must not have a generation retroactively minted"
        );
        assert_eq!(crate::wal_generation::read_generation(tmp.path()), None);
    }

    #[test]
    fn writer_over_pre_existing_generation_adopts_it() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = crate::wal_generation::ensure_generation(tmp.path()).unwrap();

        let writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        assert_eq!(writer.generation(), Some(expected.as_str()));
    }
}
