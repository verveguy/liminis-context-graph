# Parity Test Fixtures

This directory contains the request/response corpus used by the IPC parity test
(`tests/ipc_parity.rs`). Each fixture records the exact JSON-RPC wire shapes
produced by the upstream Python graphiti-core service (`graphiti_service.py`) against the `baseline_db/` snapshot.

## Directory layout

```
fixtures/
├── README.md            — this file
├── baseline_db/         — LadybugDB snapshot; generated locally, NOT committed (only .gitkeep is checked in)
│   └── liminis.db       — produced by the capture script (see below); absent until you run it
├── ipc_corpus/          — 14 request/response fixture files
│   ├── build_indices_01.json
│   ├── add_episode_01.json
│   ├── find_entities_01.json
│   ├── find_entities_02.json
│   ├── find_relationships_01.json
│   ├── find_relationships_02.json
│   ├── get_episodes_01.json
│   ├── delete_episode_01.json
│   ├── get_nodes_by_group_01.json
│   ├── get_edges_by_group_01.json
│   ├── get_edges_by_uuids_01.json
│   ├── query_cypher_01.json
│   ├── close_01.json
│   └── error_unknown_method_01.json
└── golden_queries.json  — 50-query golden set for rank-correlation (SC-002)
```

## Fixture file format

Each `.json` file in `ipc_corpus/` contains:

```json
{
  "request":  { "jsonrpc": "2.0", "id": 1, "method": "...", "params": {...} },
  "response": { "jsonrpc": "2.0", "id": 1, "result": ... }
}
```

Error responses use `"error"` instead of `"result"`.

## Capture procedure

### Prerequisites

1. Upstream Python graphiti-core service (`graphiti_service.py`) running against a fresh database
2. Python `record_corpus.py` script (see below)
3. `crates/core/tests/fixtures/baseline_db/` empty or absent

### Steps

```bash
# 1. Start the Python service against a fresh DB
LCG_DB_PATH=/tmp/baseline.db python graphiti_service.py &
PYTHON_PID=$!

# 2. Run the capture script
cd crates/core
python scripts/record_corpus.py \
  --socket /tmp/lcg/service.sock \
  --output tests/fixtures/ipc_corpus/ \
  --golden tests/fixtures/golden_queries.json

# 3. Copy the baseline DB
cp /tmp/baseline.db tests/fixtures/baseline_db/liminis.db

# 4. Stop Python service
kill $PYTHON_PID

# 5. Commit
git add tests/fixtures/
git commit -m "test(corpus): capture IPC parity fixtures from Python service"
```

### Capture script (`scripts/record_corpus.py`)

The capture script sends each IPC method to the Python service and saves the
request+response pair. It covers all 11 wire methods plus an unknown-method
error case.

## Updating fixtures

After a Python service schema change, re-run the capture procedure from scratch
and commit the updated corpus. The `PARITY_GOLDEN=1` env var enables the
rank-correlation test (SC-002); it is skipped in CI until the golden set is
captured from the Python baseline.

## CI behaviour

- Parity fixtures are committed; CI runs `cargo test --test ipc_parity` on
  every push.
- `PARITY_GOLDEN` is NOT set in CI — the golden rank-correlation test
  requires the Python-captured baseline DB and is run offline only.
