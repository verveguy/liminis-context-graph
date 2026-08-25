use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

#[path = "common/mod.rs"]
mod bench_common;
use bench_common::setup_bench_db_n;

/// Baseline (issue #219): the old `lower(e.name) = $x` Cypher predicate lbug cannot route
/// through any index — a full `Entity` table scan on every call. Issued directly via raw
/// Cypher (bypassing `get_entity_by_name_ci`, which no longer executes this query) so the
/// "before" figure stays measurable regardless of what the fixed implementation does
/// internally. Values are interpolated directly since this is a fixed benchmark string, not
/// user input.
fn bench_name_lookup_scan_baseline_10k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(10_000, dim);

    c.bench_function("name_lookup_scan_baseline_10k_hit", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let rows = conn
                .query_cypher_raw(
                    "MATCH (e:Entity) WHERE lower(e.name) = 'entity 9999' AND e.group_id = 'bench' \
                     RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, e.summary, \
                     e.attributes ORDER BY e.created_at ASC, e.uuid ASC LIMIT 1",
                )
                .unwrap();
            let _: Vec<_> = rows.collect();
        });
    });

    c.bench_function("name_lookup_scan_baseline_10k_miss", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let rows = conn
                .query_cypher_raw(
                    "MATCH (e:Entity) WHERE lower(e.name) = 'no such entity' AND e.group_id = 'bench' \
                     RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, e.summary, \
                     e.attributes ORDER BY e.created_at ASC, e.uuid ASC LIMIT 1",
                )
                .unwrap();
            let _: Vec<_> = rows.collect();
        });
    });
}

/// After (issue #221): `get_entity_by_name_ci` resolved via an equality lookup on the
/// materialized `Entity.lookup_key` column and its secondary ART index — no `Entity` table
/// scan, and (unlike the in-process `NameIndex` accelerator this replaces, issue #219) no
/// separate `get_entity_by_uuid` verify-on-hit round trip either, since the database is now
/// the sole source of truth for the lookup. `setup_bench_db_n` populates `Entity` via
/// `insert_entity`, which writes `lookup_key` on every row, and `build_indices_and_constraints`
/// builds `entity_lookup_key_idx` before this benchmark runs.
fn bench_name_lookup_art_indexed_10k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(10_000, dim);

    c.bench_function("name_lookup_art_indexed_10k_hit", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn.get_entity_by_name_ci("Entity 9999", "bench").unwrap();
        });
    });

    c.bench_function("name_lookup_art_indexed_10k_miss", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn
                .get_entity_by_name_ci("No Such Entity", "bench")
                .unwrap();
        });
    });
}

/// Measures `get_entity_by_name_ci_with_scan_fallback`'s scan-fallback path itself (issue
/// #221's Site 1 authority guarantee, `db.rs::scan_entity_by_name_ci`) at a realistic
/// embedding width (768, bge-base-en-v1.5 — the production default; the other benches in this
/// file use `dim = 8` and never exercise this path, so they cannot see this regression). The
/// indexed lookup is made to miss by deleting the row's `lookup_key` out from under it (the
/// exact out-of-band-write shape this fallback exists to catch), forcing every call through
/// the full-group scan. Before the fix that narrowed `scan_entity_by_name_ci`'s `RETURN` to
/// scalar columns, this scan transferred and sorted every row's 768-dim `name_embedding`,
/// `summary`, and `attributes`; after the fix it transfers only `uuid`/`name`/`group_id`/
/// `created_at` and hydrates at most one row via a point lookup.
fn bench_name_lookup_scan_fallback_10k(c: &mut Criterion) {
    let dim = 768;
    let (db, _dir) = setup_bench_db_n(10_000, dim);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("MATCH (e:Entity) SET e.lookup_key = NULL")
            .unwrap();
    }

    // The scan fallback self-heals a hit's `lookup_key` on the way out, so a naive `b.iter`
    // would only exercise the scan on its first iteration — every iteration after that would
    // hit the ART index directly. `iter_batched` re-nulls just this one row's `lookup_key`
    // before every iteration so each call genuinely falls through to the scan.
    c.bench_function("name_lookup_scan_fallback_10k_hit", |b| {
        b.iter_batched(
            || {
                let conn = db.connect().unwrap();
                conn.run_cypher(
                    "MATCH (e:Entity {name: 'Entity 9999', group_id: 'bench'}) \
                     SET e.lookup_key = NULL",
                )
                .unwrap();
            },
            |()| {
                let conn = db.connect().unwrap();
                let _ = conn
                    .get_entity_by_name_ci_with_scan_fallback("Entity 9999", "bench")
                    .unwrap();
            },
            BatchSize::PerIteration,
        );
    });

    // A miss never has a `lookup_key` to self-heal, so every iteration already falls through
    // to the scan — no per-iteration reset needed.
    c.bench_function("name_lookup_scan_fallback_10k_miss", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn
                .get_entity_by_name_ci_with_scan_fallback("No Such Entity", "bench")
                .unwrap();
        });
    });
}

criterion_group!(
    name_lookup,
    bench_name_lookup_scan_baseline_10k,
    bench_name_lookup_art_indexed_10k,
    bench_name_lookup_scan_fallback_10k
);
criterion_main!(name_lookup);
