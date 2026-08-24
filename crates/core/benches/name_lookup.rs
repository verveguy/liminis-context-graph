use criterion::{criterion_group, criterion_main, Criterion};

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

criterion_group!(
    name_lookup,
    bench_name_lookup_scan_baseline_10k,
    bench_name_lookup_art_indexed_10k
);
criterion_main!(name_lookup);
