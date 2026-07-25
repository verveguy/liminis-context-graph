#!/usr/bin/env python3
"""
capture_real_corpus.py — capture the golden real-corpus WAL fixture for issue #217.

This is a one-time, offline, OUTSIDE-CI step (see specs/217-golden-real-corpus-wal/spec.md,
Assumptions). It drives a real, running `liminis-context-graph` service instance — configured
with the real `AnthropicExtractor` (via `ANTHROPIC_API_KEY`) and the real `OaiEmbedder` (the
local CoreML sidecar, `native/local-inference/`) — over its Unix socket, ingesting the pinned
Simple English Wikipedia corpus in `corpus_manifest.json` one article per episode. When ingest
completes it captures:

  - the resulting WAL, concatenated across files (lexicographic order, matching
    `WalReplayer`'s own read order) and gzip-compressed to `wal.jsonl.gz`
  - `expected_results.json`: recorded counts, embedding dim, golden queries, a 2-hop
    traversal path, and relation-type samples — the fixture's "expected results record"

Prerequisites (all one-time/local, never required in CI):
  1. `ANTHROPIC_API_KEY` set in the environment (real LLM extraction).
  2. The local embedding sidecar reachable — either the default UDS socket
     (`/tmp/liminis-inference.sock`) or `LCG_EMBEDDING_URL` pointed at an HTTP endpoint.
  3. The compiled `liminis-context-graph` binary (`cargo build --release`).
  4. Network access to `simple.wikipedia.org` (to fetch pinned article revisions).

Usage:
    # 1. Build the release binary
    cargo build --release -p lcg-service

    # 2. Start it against a fresh, empty DB/WAL dir
    export ANTHROPIC_API_KEY=sk-...
    LCG_DB_PATH=/tmp/real_corpus_capture/db \\
    LCG_WAL_DIR=/tmp/real_corpus_capture/wal \\
    LCG_SOCKET_PATH=/tmp/real_corpus_capture/service.sock \\
      ./target/release/liminis-context-graph &

    # 3. Run this script
    python3 crates/core/scripts/capture_real_corpus.py \\
        --socket /tmp/real_corpus_capture/service.sock \\
        --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \\
        --wal-dir /tmp/real_corpus_capture/wal \\
        --output-dir crates/core/tests/fixtures/real_corpus_wal

    # 4. Stop the service, review, commit the fixture files
    kill %1
    git add crates/core/tests/fixtures/real_corpus_wal/wal.jsonl.gz \\
            crates/core/tests/fixtures/real_corpus_wal/expected_results.json
    git commit -m "test(corpus): capture golden real-corpus WAL fixture (#217)"

The script fails loudly (non-zero exit) on any article fetch or ingest error rather than
silently skipping articles — a partial/silently-degraded corpus would defeat the point of the
fixture (see the Research-stage "Risks" note on reproducibility).
"""

import argparse
import gzip
import json
import re
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

WIKI_API = "https://simple.wikipedia.org/w/api.php"
USER_AGENT = "liminis-context-graph-fixture-capture/1.0 (https://github.com/verveguy/liminis-context-graph)"

# Representative "hub" entities expected to recur densely across the Apollo-program corpus
# (FR-001c) — used to build the golden-query and traversal assertions in expected_results.json.
# These are article titles from corpus_manifest.json, not guaranteed entity names post-extraction;
# the script searches for them via knowledge_find_entities after ingest and records whatever the
# real extractor actually produced.
HUB_QUERY_SEEDS = [
    "NASA",
    "Apollo 11",
    "Neil Armstrong",
    "Buzz Aldrin",
    "Wernher von Braun",
    "Kennedy Space Center",
    "Saturn V",
    "Apollo program",
]

RELATIONSHIP_QUERY_SEEDS = [
    "worked at NASA",
    "walked on the Moon",
    "launched from Kennedy Space Center",
    "commanded the mission",
]


# ── Wikipedia fetch ────────────────────────────────────────────────────────────


def fetch_json(url, retries=5, backoff=8.0):
    """GETs `url` with a descriptive User-Agent, retrying on 429 with backoff.

    Wikimedia's API rate-limits aggressively for anonymous/bot-like traffic (observed
    empirically while building corpus_manifest.json) — retry rather than fail on 429.
    """
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            if e.code == 429 and attempt < retries - 1:
                time.sleep(backoff * (attempt + 1))
                continue
            raise


def fetch_wikitext(revision_id: int) -> str:
    url = (
        f"{WIKI_API}?action=query&prop=revisions&rvprop=content&rvslots=main"
        f"&revids={revision_id}&format=json"
    )
    data = fetch_json(url)
    pages = data.get("query", {}).get("pages", {})
    for page in pages.values():
        revisions = page.get("revisions", [])
        if not revisions:
            raise RuntimeError(f"revision {revision_id} has no content (deleted/moved?)")
        return revisions[0]["slots"]["main"]["*"]
    raise RuntimeError(f"revision {revision_id} not found in API response")


# ── Wikitext → plain prose (good-enough for extraction, not a full parser) ────

_REF_RE = re.compile(r"<ref[^>]*>.*?</ref>|<ref[^/>]*/>", re.DOTALL | re.IGNORECASE)
_COMMENT_RE = re.compile(r"<!--.*?-->", re.DOTALL)
_HTML_TAG_RE = re.compile(r"<[^>]+>")
_PIPED_LINK_RE = re.compile(r"\[\[([^\]|]*)\|([^\]]*)\]\]")
_PLAIN_LINK_RE = re.compile(r"\[\[([^\]]*)\]\]")
_EXTERNAL_LINK_RE = re.compile(r"\[https?://[^\s\]]+\s+([^\]]*)\]")
_BOLD_ITALIC_RE = re.compile(r"'{2,5}")
_HEADING_RE = re.compile(r"^=+\s*(.*?)\s*=+$", re.MULTILINE)
_FILE_LINK_RE = re.compile(r"\[\[(File|Image|Category):[^\]]*\]\]", re.IGNORECASE)


def strip_templates(text: str) -> str:
    """Removes balanced {{ ... }} templates (infoboxes, citations, etc.)."""
    out = []
    depth = 0
    i = 0
    while i < len(text):
        if text[i : i + 2] == "{{":
            depth += 1
            i += 2
            continue
        if text[i : i + 2] == "}}" and depth > 0:
            depth -= 1
            i += 2
            continue
        if depth == 0:
            out.append(text[i])
        i += 1
    return "".join(out)


def wikitext_to_prose(wikitext: str) -> str:
    text = _COMMENT_RE.sub("", wikitext)
    text = _REF_RE.sub("", text)
    text = strip_templates(text)
    text = _FILE_LINK_RE.sub("", text)
    text = _PIPED_LINK_RE.sub(r"\2", text)
    text = _PLAIN_LINK_RE.sub(r"\1", text)
    text = _EXTERNAL_LINK_RE.sub(r"\1", text)
    text = _BOLD_ITALIC_RE.sub("", text)
    text = _HEADING_RE.sub(r"\1.", text)
    text = _HTML_TAG_RE.sub("", text)
    # Collapse excess blank lines/whitespace left behind by stripped markup.
    lines = [ln.strip() for ln in text.splitlines()]
    lines = [ln for ln in lines if ln]
    return "\n".join(lines)


# ── Service socket client ──────────────────────────────────────────────────────


class ServiceClient:
    def __init__(self, socket_path: str):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(socket_path)
        self.reader = self.sock.makefile("r", encoding="utf-8")
        self.next_id = 1

    def call(self, method: str, params: dict, timeout: float = 120.0):
        req_id = self.next_id
        self.next_id += 1
        self.sock.settimeout(timeout)
        line = json.dumps({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        self.sock.sendall((line + "\n").encode("utf-8"))
        raw = self.reader.readline()
        if not raw:
            raise ConnectionError(f"socket closed while waiting for response to {method}")
        resp = json.loads(raw)
        if "error" in resp:
            raise RuntimeError(f"{method} failed: {resp['error']}")
        return resp["result"]

    def close(self):
        self.sock.close()


# ── Capture steps ───────────────────────────────────────────────────────────────


def ingest_corpus(client: ServiceClient, articles: list, group_id: str, wiki_delay: float):
    for i, article in enumerate(articles, start=1):
        title = article["title"]
        revision_id = article["revision_id"]
        print(f"[{i}/{len(articles)}] fetching {title!r} (rev {revision_id})...", flush=True)
        wikitext = fetch_wikitext(revision_id)
        prose = wikitext_to_prose(wikitext)
        if len(prose) < 200:
            raise RuntimeError(
                f"article {title!r} (rev {revision_id}) produced suspiciously short prose "
                f"({len(prose)} chars) after wikitext cleanup — check the cleanup regexes"
            )

        ref_time = article["revision_timestamp"]
        result = client.call(
            "knowledge_add_episode",
            {
                "name": title,
                "episode_body": prose,
                "source": "text",
                "source_description": f"Simple English Wikipedia: {title} (rev {revision_id})",
                "reference_time": ref_time,
                "group_id": group_id,
            },
        )
        print(f"    -> episode {result.get('episode_uuid')}", flush=True)
        time.sleep(wiki_delay)


def build_expected_results(client: ServiceClient, group_id: str) -> dict:
    status = client.call("knowledge_status", {})

    golden_entity_queries = []
    hub_candidate = None
    for query in HUB_QUERY_SEEDS:
        result = client.call(
            "knowledge_find_entities",
            {"query": query, "group_ids": [group_id], "num_results": 10},
        )
        nodes = result.get("nodes", [])
        golden_entity_queries.append(
            {
                "query": query,
                "expected_top_n": 10,
                "expected_entity_names": [n.get("name") for n in nodes],
                "expected_entity_uuids": [n.get("uuid") for n in nodes],
            }
        )
        if nodes and hub_candidate is None:
            hub_candidate = nodes[0]

    golden_relationship_queries = []
    for query in RELATIONSHIP_QUERY_SEEDS:
        result = client.call(
            "knowledge_find_relationships",
            {"query": query, "group_ids": [group_id], "num_results": 10},
        )
        facts = result.get("facts", [])
        golden_relationship_queries.append(
            {
                "query": query,
                "expected_top_n": 10,
                "expected_fact_uuids": [f.get("uuid") for f in facts],
                "expected_relation_types": [f.get("relation_type") for f in facts],
            }
        )

    traversal = None
    if hub_candidate is not None:
        hop1 = client.call(
            "knowledge_get_entity_neighbors",
            {"entity_uuid": hub_candidate["uuid"], "group_ids": [group_id], "num_results": 25},
        )
        hop1_nodes = [n for n in hop1.get("nodes", []) if n.get("uuid") != hub_candidate["uuid"]]
        if hop1_nodes:
            hop2 = client.call(
                "knowledge_get_entity_neighbors",
                {
                    "entity_uuid": hop1_nodes[0]["uuid"],
                    "group_ids": [group_id],
                    "num_results": 25,
                },
            )
            traversal = {
                "start_entity_uuid": hub_candidate["uuid"],
                "start_entity_name": hub_candidate.get("name"),
                "hop1_entity_uuid": hop1_nodes[0]["uuid"],
                "hop1_entity_name": hop1_nodes[0].get("name"),
                "expected_hop1_node_uuids": sorted(n["uuid"] for n in hop1.get("nodes", [])),
                "expected_hop2_node_uuids": sorted(
                    n["uuid"] for n in hop2.get("nodes", [])
                ),
            }

    relationships = client.call(
        "knowledge_list_relationships", {"group_ids": [group_id], "num_results": 1000}
    )
    edges = relationships.get("edges", [])
    relation_type_samples = [
        {
            "uuid": e.get("uuid"),
            "fact": e.get("fact"),
            "relation_type": e.get("relation_type") or e.get("name"),
        }
        for e in edges[:50]
        if e.get("fact")
    ]

    return {
        "_comment": (
            "Expected-results record for the #217 golden real-corpus WAL fixture. "
            "Generated by capture_real_corpus.py at capture time; consumed by "
            "crates/core/tests/real_corpus_e2e.rs. Golden-query assertions are written as "
            "top-N set-membership (see real_corpus_wal/README.md) because query-time "
            "embedding in the e2e test uses MockEmbedder (FR-011 zero-network), so only the "
            "BM25/FTS half of RRF fusion is deterministic-and-informative at test time."
        ),
        "group_id": group_id,
        "embedding_dim": status["embedding_dim"],
        "entity_count": status["entity_count"],
        "relationship_count": status["relationship_count"],
        "episode_count": status["episode_count"],
        "indices_built": status["indices_built"],
        "golden_entity_queries": golden_entity_queries,
        "golden_relationship_queries": golden_relationship_queries,
        "traversal": traversal,
        "relation_type_samples": relation_type_samples,
    }


def gzip_wal_dir(wal_dir: str, output_path: str):
    import os

    files = sorted(f for f in os.listdir(wal_dir) if f.endswith(".jsonl"))
    if not files:
        raise RuntimeError(f"no .jsonl WAL files found in {wal_dir}")
    print(f"concatenating {len(files)} WAL files (lexicographic order) -> {output_path}")
    with gzip.open(output_path, "wb") as out:
        for fname in files:
            with open(os.path.join(wal_dir, fname), "rb") as f:
                out.write(f.read())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--socket", required=True, help="Path to the running service's Unix socket")
    parser.add_argument("--manifest", required=True, help="Path to corpus_manifest.json")
    parser.add_argument("--wal-dir", required=True, help="WAL directory the running service writes to")
    parser.add_argument("--output-dir", required=True, help="real_corpus_wal/ fixture directory")
    parser.add_argument("--group-id", default="apollo_program", help="group_id used for all episodes")
    parser.add_argument(
        "--wiki-delay",
        type=float,
        default=1.0,
        help="seconds to sleep between Wikipedia API fetches (rate-limit friendliness)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="cap the number of articles ingested (for a quick smoke-test run)",
    )
    args = parser.parse_args()

    with open(args.manifest) as f:
        manifest = json.load(f)
    articles = manifest["articles"]
    if args.limit:
        articles = articles[: args.limit]

    start = time.monotonic()
    client = ServiceClient(args.socket)
    try:
        ingest_corpus(client, articles, args.group_id, args.wiki_delay)
        elapsed = time.monotonic() - start
        print(f"ingest complete in {elapsed:.1f}s, building expected_results.json...")

        expected = build_expected_results(client, args.group_id)
        expected["capture_wall_clock_seconds"] = round(elapsed, 1)
        expected["article_count"] = len(articles)

        if expected["entity_count"] <= 1000:
            print(
                f"WARNING: entity_count ({expected['entity_count']}) does not exceed the "
                "default hybrid-dedup threshold (1000) — SC-002 requires margin above it. "
                "Extend corpus_manifest.json with more articles and re-run.",
                file=sys.stderr,
            )
    finally:
        client.close()

    import os

    os.makedirs(args.output_dir, exist_ok=True)
    expected_path = os.path.join(args.output_dir, "expected_results.json")
    with open(expected_path, "w") as f:
        json.dump(expected, f, indent=2)
        f.write("\n")
    print(f"wrote {expected_path}")

    wal_gz_path = os.path.join(args.output_dir, "wal.jsonl.gz")
    gzip_wal_dir(args.wal_dir, wal_gz_path)
    print(f"wrote {wal_gz_path}")

    print(
        f"\nCapture complete: {expected['entity_count']} entities, "
        f"{expected['relationship_count']} relationships, {expected['episode_count']} episodes."
    )


if __name__ == "__main__":
    main()
