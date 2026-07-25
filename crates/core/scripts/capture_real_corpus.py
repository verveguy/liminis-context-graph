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
  - `corpus_prose.jsonl.gz`: the cleaned prose actually fed to the extractor for every
    consumed article, one JSON record per article (`{title, revision_id, prose}`) behind a
    header record (`{cleanup_version, script_git_sha, record_count}`) — see "The four
    fixture artifacts" below

Ingest stops as soon as `entity_count` reaches `--target-entities` (default 1500 —
comfortably past the 1000 hybrid-dedup threshold with headroom), rather than always consuming
the full manifest. In a tight domain cluster, recurring hub entities dedup heavily, so unique
entity count grows sublinearly with article count — polling `knowledge_status` after each
article and stopping early avoids paying for extraction on articles the fixture doesn't need.
The manifest's remaining, unconsumed entries are left in place (a documented way to grow the
fixture later without re-curating); the actually-consumed prefix (and the skipped-stub list) is
recorded in `expected_results.json` and back into `corpus_manifest.json` (`last_capture`,
including the full consumed/skipped article lists) for reproducibility.

The four fixture artifacts and what each is for:

  | Artifact                 | Purpose                                                    |
  |---------------------------|------------------------------------------------------------|
  | `corpus_manifest.json`   | provenance — pinned revisions, reproducibility             |
  | `corpus_prose.jsonl.gz`  | re-extract the same inputs with a different model (#228)   |
  | `wal.jsonl.gz`           | rebuild the graph deterministically, zero LLM calls         |
  | (future) LLM cassette    | replay one model's exact request/response exchange          |

`wal.jsonl.gz` is *post-extraction*, so it cannot test a different extractor; a future cassette
records one model's exchanges, so it cannot test a different model either. Only the raw prose
that was fed in supports an apples-to-apples extraction-model comparison — and refetching from
Wikipedia at compare-time is not equivalent even with pinned revisions, because the prose is a
derived artifact of `wikitext_to_prose` (see `CLEANUP_VERSION` below): a future cleanup change
would silently change the compared input out from under a model-vs-model eval. `corpus_prose.jsonl.gz`
is written automatically at the end of a normal capture run (from the already-fetched consumed
articles, no extra network calls beyond the ingest itself); `--prose-only` regenerates it later,
standalone, from `corpus_manifest.json`'s `last_capture.consumed_articles` — no service, no
`ANTHROPIC_API_KEY`, no embedder, costing only Wikipedia re-fetches.

Prerequisites (all one-time/local, never required in CI):
  1. `ANTHROPIC_API_KEY` set in the environment (real LLM extraction).
  2. The local embedding sidecar reachable — either the default UDS socket
     (`/tmp/liminis-inference.sock`) or `LCG_EMBEDDING_URL` pointed at an HTTP endpoint.
     On a fresh checkout the sidecar compiles its `.mlpackage` fine but then fails with
     `offlineModeError("Repository not available locally")` fetching the
     `BAAI/bge-base-en-v1.5` tokenizer — it doesn't look in the repo by default even
     though the tokenizer ships in the embedding-assets release tarball. Point it there:
     `export LOCAL_INFERENCE_HF_CACHE=<repo>/resources/models/tokenizer`.
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
            crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl.gz \\
            crates/core/tests/fixtures/real_corpus_wal/expected_results.json \\
            crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json
    git commit -m "test(corpus): capture golden real-corpus WAL fixture (#217)"

Pass --target-entities to override the default 1500-entity stopping point (e.g.
--target-entities 5000 for a deliberately larger fixture, or a smoke-test run with
--limit 20 --target-entities 50 to sanity-check the pipeline cheaply before a full capture).

To regenerate ONLY `corpus_prose.jsonl.gz` later (e.g. after extending the manifest and
re-capturing, or if the committed file is lost/corrupted) without repeating LLM extraction:

    python3 crates/core/scripts/capture_real_corpus.py \\
        --prose-only \\
        --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \\
        --output-dir crates/core/tests/fixtures/real_corpus_wal

This requires no `--socket`/`--wal-dir` (no running service), no `ANTHROPIC_API_KEY`, and no
embedder — it reads the consumed-article list from `corpus_manifest.json`'s
`last_capture.consumed_articles` (written by a prior full capture run) and re-runs the same
`wikitext_to_prose` over freshly refetched pinned revisions. If `wikitext_to_prose` (or its
helpers) has changed since the committed `corpus_prose.jsonl.gz` was captured, bump
`CLEANUP_VERSION` below so the mismatch is detectable from the file's header rather than silent.

The script fails loudly (non-zero exit) on any article fetch, ingest, or service error — a
partial/silently-degraded corpus would defeat the point of the fixture (see the Research-stage
"Risks" note on reproducibility). The one exception is an individual article whose wikitext
cleans up to suspiciously short prose (<200 chars): that's skipped and recorded in
`expected_results.json`'s `skipped_articles`, not a hard abort, since Simple English Wikipedia
has many genuine stub articles and a real, paid extraction run shouldn't throw away everything
already ingested over one stub. The run still aborts if skipped articles exceed ~20% of
attempted articles (see `SKIP_RATIO_THRESHOLD`) — that's a systemic cleanup regression, not
stub noise.

Resumable: at startup the script queries which episodes already exist for `--group-id` and
skips re-fetching/re-extracting those manifest articles. If a run aborts partway (a service
crash, a systemic-skip abort, etc.), re-running the same command against the *same* (not a
fresh) DB/WAL dir picks up where it left off instead of re-paying for already-ingested articles.
"""

import argparse
import gzip
import json
import os
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

WIKI_API = "https://simple.wikipedia.org/w/api.php"
USER_AGENT = "liminis-context-graph-fixture-capture/1.0 (https://github.com/verveguy/liminis-context-graph)"

# Bump this whenever `wikitext_to_prose` or any helper it calls (`strip_templates`,
# `strip_file_links`, the ref/link/heading regexes) changes behavior. It's recorded in
# `corpus_prose.jsonl.gz`'s header record so a future extraction-model comparison (#228) can
# detect that the committed prose predates a cleanup fix, rather than silently comparing
# against stale/incorrect input text (see #217: three cleanup bugs were fixed during this
# issue alone — ref/template ordering, stub handling, nested file-links).
#   1 - initial (buggy) implementation: ref-before-template ordering, regex-based file-link
#       stripping that broke on nested `[[...]]` captions.
#   2 - current: template-before-ref ordering, stack-based `strip_file_links`.
CLEANUP_VERSION = 2

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

# Self-closing form MUST be tried first: with the closed-ref-content alternative first,
# a self-closing tag like `<ref name=x />` still matches it (the `.*?` is happy to match
# zero chars up to `>`), then greedily searches for the *next* `</ref>` anywhere later in
# the document (DOTALL) and swallows everything in between — silently dropping prose that
# has nothing to do with the self-closing tag itself. Trying `<ref[^/>]*/>` first prevents
# that same-tag ambiguity from ever reaching the greedy branch.
_REF_RE = re.compile(r"<ref[^/>]*/>|<ref[^>]*>.*?</ref>", re.DOTALL | re.IGNORECASE)
_COMMENT_RE = re.compile(r"<!--.*?-->", re.DOTALL)
_HTML_TAG_RE = re.compile(r"<[^>]+>")
_PIPED_LINK_RE = re.compile(r"\[\[([^\]|]*)\|([^\]]*)\]\]")
_PLAIN_LINK_RE = re.compile(r"\[\[([^\]]*)\]\]")
_EXTERNAL_LINK_RE = re.compile(r"\[https?://[^\s\]]+\s+([^\]]*)\]")
_BOLD_ITALIC_RE = re.compile(r"'{2,5}")
_HEADING_RE = re.compile(r"^=+\s*(.*?)\s*=+$", re.MULTILINE)
_FILE_LINK_START_RE = re.compile(r"\[\[(File|Image|Category)\s*:", re.IGNORECASE)


def strip_templates(text: str) -> str:
    """Removes balanced {{ ... }} templates (infoboxes, citations, etc.).

    Real wikitext is not guaranteed to have perfectly balanced braces by the time this
    runs (a malformed template, or upstream cleanup that strips a `}}` along with its
    surrounding markup, can leave a `{{` with no partner). A naive depth counter that
    only emits text at depth == 0 treats an unmatched opener as "still inside a
    template" for the rest of the string, silently dropping everything after it —
    which previously turned one bad brace into an entire missing article (see #217).

    Matching is stack-based instead: a `{{`/`}}` pair is only removed once a partner is
    actually found, so an unmatched `{{` is left as literal text and stripping recovers
    on the next real match rather than swallowing the remainder of the document.
    """
    stack = []
    remove_spans = []
    i = 0
    n = len(text)
    while i < n:
        if text[i : i + 2] == "{{":
            stack.append(i)
            i += 2
            continue
        if text[i : i + 2] == "}}" and stack:
            start = stack.pop()
            remove_spans.append((start, i + 2))
            i += 2
            continue
        i += 1

    remove_spans.sort()
    merged = []
    for start, end in remove_spans:
        if merged and start < merged[-1][1]:
            if end > merged[-1][1]:
                merged[-1] = (merged[-1][0], end)
            continue
        merged.append((start, end))

    out = []
    pos = 0
    for start, end in merged:
        out.append(text[pos:start])
        pos = end
    out.append(text[pos:])
    return "".join(out)


def strip_file_links(text: str) -> str:
    """Removes balanced [[File:...]] / [[Image:...]] / [[Category:...]] links, including
    any nested [[...]] wiki links inside a caption (e.g.
    `[[File:x.jpg|thumb|the [[Saturn V]] rocket]]`).

    The previous implementation (`\\[\\[(File|Image|Category):[^\\]]*\\]\\]`) assumed the
    caption contained no further `[[...]]` link — but Simple English Wikipedia captions
    routinely link other articles inside the caption text. The regex's `[^\\]]*` stopped
    at the *first* `]]` it found, which was the inner link's closer, not the file link's
    own closer, leaving a debris tail like `-I, Bhaskara-II and Aryabhata satellites]]` in
    the prose (see #217, "Aryabhata (satellite)" rev 10314108).

    Matching is stack-based, mirroring `strip_templates`: only a `[[`/`]]` pair that
    actually finds its partner is removed, so an unmatched `[[` is left as literal text
    instead of silently swallowing the rest of the document.
    """
    stack = []  # list of (start_index, is_file_link)
    remove_spans = []
    i = 0
    n = len(text)
    while i < n:
        if text[i : i + 2] == "[[":
            is_file = bool(_FILE_LINK_START_RE.match(text, i))
            stack.append((i, is_file))
            i += 2
            continue
        if text[i : i + 2] == "]]" and stack:
            start, is_file = stack.pop()
            if is_file:
                remove_spans.append((start, i + 2))
            i += 2
            continue
        i += 1

    if not remove_spans:
        return text

    remove_spans.sort()
    merged = []
    for start, end in remove_spans:
        if merged and start < merged[-1][1]:
            if end > merged[-1][1]:
                merged[-1] = (merged[-1][0], end)
            continue
        merged.append((start, end))

    out = []
    pos = 0
    for start, end in merged:
        out.append(text[pos:start])
        pos = end
    out.append(text[pos:])
    return "".join(out)


def wikitext_to_prose(wikitext: str) -> str:
    text = _COMMENT_RE.sub("", wikitext)
    # Templates are stripped before refs: braces are balanced in raw wikitext, but a
    # <ref>...</ref> can itself contain a template (e.g. `<ref>{{Cite web|...}}</ref>`)
    # whose `}}` sits inside the ref while a sibling template's matching `{{` sits
    # outside it — removing the ref first can leave that sibling unbalanced.
    text = strip_templates(text)
    text = _REF_RE.sub("", text)
    text = strip_file_links(text)
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

# A too-short article aborts the whole run only once skipped articles exceed this share
# of attempted articles (and a minimum sample has been attempted, so one early stub
# doesn't trip the guard) — see #217: an isolated short article is normal stub-article
# noise on Simple English Wikipedia, not a cleanup regression.
SKIP_RATIO_THRESHOLD = 0.20
SKIP_RATIO_MIN_SAMPLE = 10


def fetch_already_ingested_titles(client: ServiceClient, group_id: str) -> set:
    """Returns the episode `name`s already present for `group_id`.

    Lets a re-run of this script (pointed at a non-fresh DB/WAL dir left over from a
    prior aborted run) skip re-fetching and re-extracting articles it already paid for,
    instead of re-paying for the whole prefix on every retry (see #217).
    """
    result = client.call("knowledge_get_episodes", {"group_id": group_id, "last_n": 10000})
    return {ep.get("name") for ep in result.get("episodes", []) if ep.get("name")}


def ingest_corpus(
    client: ServiceClient,
    articles: list,
    group_id: str,
    wiki_delay: float,
    target_entities: int,
    already_ingested_titles: set = None,
) -> tuple:
    """Ingests `articles` in manifest order, polling `knowledge_status` after each newly
    added episode and stopping as soon as `entity_count >= target_entities`. Returns
    `(consumed, skipped)`:

      - `consumed`: articles that contributed an episode to the graph, either just now or
        in a prior run (`already_ingested_titles`), in manifest order — so the caller can
        record exactly what built the fixture.
      - `skipped`: articles skipped because wikitext cleanup produced suspiciously short
        prose, each as `{title, revision_id, prose_chars}`.

    A too-short article is skipped rather than aborting the whole run: prose length varies
    legitimately (Simple English Wikipedia has many genuine stub articles — e.g.
    "Aryabhata (satellite)", 154 chars, is entirely correct output), and a hard abort
    throws away every dollar already spent on real LLM extraction for prior articles in
    this run (see #217). The run still aborts on short prose once skipped articles exceed
    `SKIP_RATIO_THRESHOLD` of attempted articles — that's a systemic cleanup regression,
    not stub-article noise. Non-recoverable errors (network/ingest/service failures) still
    abort immediately; skipping only applies to the short-prose heuristic.

    Raises if the entire manifest is exhausted without reaching `target_entities` — that
    means the manifest needs more articles, not a silently-undersized fixture.
    """
    already = already_ingested_titles or set()
    consumed = []
    skipped = []
    entity_count = None
    for i, article in enumerate(articles, start=1):
        title = article["title"]
        revision_id = article["revision_id"]

        if title in already:
            print(
                f"[{i}/{len(articles)}] {title!r} already ingested (resuming) — "
                "skipping fetch/extract",
                flush=True,
            )
            consumed.append(article)
            continue

        print(f"[{i}/{len(articles)}] fetching {title!r} (rev {revision_id})...", flush=True)
        wikitext = fetch_wikitext(revision_id)
        prose = wikitext_to_prose(wikitext)
        if len(prose) < 200:
            skipped.append(
                {"title": title, "revision_id": revision_id, "prose_chars": len(prose)}
            )
            attempted = len(consumed) + len(skipped)
            print(
                f"    SKIP: only {len(prose)} chars of prose after wikitext cleanup "
                f"(likely a genuine stub article) — {len(skipped)}/{attempted} attempted "
                "articles skipped so far",
                flush=True,
            )
            if (
                attempted >= SKIP_RATIO_MIN_SAMPLE
                and len(skipped) / attempted > SKIP_RATIO_THRESHOLD
            ):
                raise RuntimeError(
                    f"{len(skipped)}/{attempted} attempted articles skipped for short prose "
                    f"(>{SKIP_RATIO_THRESHOLD:.0%}) — this looks like a systemic wikitext "
                    "cleanup regression, not normal stub-article noise. Skipped so far: "
                    f"{[s['title'] for s in skipped]}"
                )
            time.sleep(wiki_delay)
            continue

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
        consumed.append(article)
        status = client.call("knowledge_status", {})
        entity_count = status["entity_count"]
        print(
            f"    -> episode {result.get('episode_uuid')}, entity_count={entity_count}",
            flush=True,
        )
        if entity_count >= target_entities:
            print(
                f"reached target_entities={target_entities} after {len(consumed)}/{len(articles)} "
                "articles — stopping ingest early",
                flush=True,
            )
            return consumed, skipped
        time.sleep(wiki_delay)

    if entity_count is None:
        # Every manifest article was already ingested in a prior run (pure resume) —
        # check status directly rather than assuming the target was reached.
        status = client.call("knowledge_status", {})
        entity_count = status["entity_count"]
        if entity_count >= target_entities:
            return consumed, skipped

    raise RuntimeError(
        f"exhausted all {len(articles)} manifest articles without reaching "
        f"target_entities={target_entities} (last observed entity_count={entity_count}). "
        "Extend corpus_manifest.json with more articles and re-run."
    )


def build_expected_results(
    client: ServiceClient,
    group_id: str,
    consumed_articles: list,
    skipped_articles: list,
    target_entities: int,
    total_manifest_size: int,
) -> dict:
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
        "target_entities": target_entities,
        "consumed_article_count": len(consumed_articles),
        "manifest_article_count": total_manifest_size,
        "consumed_articles": [
            {"title": a["title"], "revision_id": a["revision_id"]} for a in consumed_articles
        ],
        "skipped_article_count": len(skipped_articles),
        "skipped_articles": skipped_articles,
        "golden_entity_queries": golden_entity_queries,
        "golden_relationship_queries": golden_relationship_queries,
        "traversal": traversal,
        "relation_type_samples": relation_type_samples,
    }


def gzip_wal_dir(wal_dir: str, output_path: str):
    files = sorted(f for f in os.listdir(wal_dir) if f.endswith(".jsonl"))
    if not files:
        raise RuntimeError(f"no .jsonl WAL files found in {wal_dir}")
    print(f"concatenating {len(files)} WAL files (lexicographic order) -> {output_path}")
    with gzip.open(output_path, "wb") as out:
        for fname in files:
            with open(os.path.join(wal_dir, fname), "rb") as f:
                out.write(f.read())


def script_git_sha() -> str:
    """Best-effort git SHA of this script's own commit, for the corpus_prose.jsonl.gz header.

    Provenance-only (see CLEANUP_VERSION for the actual staleness signal) — falls back to
    None rather than failing the capture if git isn't available (e.g. a tarball checkout).
    """
    try:
        return (
            subprocess.check_output(
                ["git", "rev-parse", "HEAD"],
                cwd=os.path.dirname(os.path.abspath(__file__)),
                stderr=subprocess.DEVNULL,
            )
            .decode()
            .strip()
        )
    except Exception:
        return None


def build_corpus_prose(consumed_articles: list, wiki_delay: float) -> list:
    """Refetches each consumed article's pinned revision and re-runs `wikitext_to_prose`,
    returning `[{title, revision_id, prose}, ...]` in manifest order.

    This is the exact text that was fed to `knowledge_add_episode` for every episode in the
    committed WAL (same pinned revisions, same cleanup function) — the input fixture for
    comparing a different extraction model against the same real corpus (#228), which neither
    the WAL (post-extraction) nor a future LLM cassette (one model's exchange only) can serve.
    Zero LLM/embedder calls; costs only Wikipedia fetches.
    """
    records = []
    for i, article in enumerate(consumed_articles, start=1):
        title = article["title"]
        revision_id = article["revision_id"]
        print(f"[{i}/{len(consumed_articles)}] fetching prose for {title!r} (rev {revision_id})...", flush=True)
        wikitext = fetch_wikitext(revision_id)
        prose = wikitext_to_prose(wikitext)
        records.append({"title": title, "revision_id": revision_id, "prose": prose})
        time.sleep(wiki_delay)
    return records


def write_corpus_prose_gz(records: list, output_path: str) -> None:
    """Writes `records` as gzip-compressed JSONL behind a header record, so
    `corpus_prose.jsonl.gz` self-documents the cleanup version it was captured against
    (see CLEANUP_VERSION) without needing `expected_results.json` open alongside it.
    """
    header = {
        "_header": True,
        "cleanup_version": CLEANUP_VERSION,
        "script_git_sha": script_git_sha(),
        "record_count": len(records),
    }
    with gzip.open(output_path, "wt", encoding="utf-8") as out:
        out.write(json.dumps(header) + "\n")
        for record in records:
            out.write(json.dumps(record) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--socket", default=None, help="Path to the running service's Unix socket (required unless --prose-only)"
    )
    parser.add_argument("--manifest", required=True, help="Path to corpus_manifest.json")
    parser.add_argument(
        "--wal-dir", default=None, help="WAL directory the running service writes to (required unless --prose-only)"
    )
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
        help="cap the number of articles considered for ingest (for a quick smoke-test run)",
    )
    parser.add_argument(
        "--target-entities",
        type=int,
        default=1500,
        help=(
            "stop ingest as soon as knowledge_status reports entity_count >= this value "
            "(default 1500 — comfortably past the 1000 hybrid-dedup threshold with headroom, "
            "without paying for extraction on articles the fixture doesn't need)"
        ),
    )
    parser.add_argument(
        "--prose-only",
        action="store_true",
        help=(
            "Skip ingest entirely and only (re)generate corpus_prose.jsonl.gz, reading the "
            "consumed-article list from corpus_manifest.json's last_capture.consumed_articles "
            "(written by a prior full capture run). No service connection, no "
            "ANTHROPIC_API_KEY, no embedder — costs only Wikipedia re-fetches."
        ),
    )
    args = parser.parse_args()

    with open(args.manifest) as f:
        manifest = json.load(f)

    if args.prose_only:
        consumed = manifest.get("last_capture", {}).get("consumed_articles")
        if not consumed:
            raise SystemExit(
                "--prose-only requires corpus_manifest.json to already have a "
                "last_capture.consumed_articles record from a prior full capture run "
                "(run this script without --prose-only first)"
            )
        os.makedirs(args.output_dir, exist_ok=True)
        records = build_corpus_prose(consumed, args.wiki_delay)
        prose_path = os.path.join(args.output_dir, "corpus_prose.jsonl.gz")
        write_corpus_prose_gz(records, prose_path)
        print(f"wrote {prose_path} ({len(records)} articles, cleanup_version={CLEANUP_VERSION})")
        return

    if not args.socket or not args.wal_dir:
        raise SystemExit("--socket and --wal-dir are required unless --prose-only is given")

    articles = manifest["articles"]
    if args.limit:
        articles = articles[: args.limit]
    total_manifest_size = len(articles)

    start = time.monotonic()
    client = ServiceClient(args.socket)
    try:
        already_ingested_titles = fetch_already_ingested_titles(client, args.group_id)
        if already_ingested_titles:
            print(
                f"resuming: {len(already_ingested_titles)} episode(s) already present for "
                f"group_id={args.group_id!r} — matching manifest articles will be skipped",
                flush=True,
            )
        consumed_articles, skipped_articles = ingest_corpus(
            client,
            articles,
            args.group_id,
            args.wiki_delay,
            args.target_entities,
            already_ingested_titles,
        )
        elapsed = time.monotonic() - start
        print(f"ingest complete in {elapsed:.1f}s, building expected_results.json...")

        expected = build_expected_results(
            client,
            args.group_id,
            consumed_articles,
            skipped_articles,
            args.target_entities,
            total_manifest_size,
        )
        expected["capture_wall_clock_seconds"] = round(elapsed, 1)

        if expected["entity_count"] <= 1000:
            print(
                f"WARNING: entity_count ({expected['entity_count']}) does not exceed the "
                "default hybrid-dedup threshold (1000) — SC-002 requires margin above it. "
                "Re-run with a higher --target-entities.",
                file=sys.stderr,
            )
    finally:
        client.close()

    import datetime

    os.makedirs(args.output_dir, exist_ok=True)
    expected_path = os.path.join(args.output_dir, "expected_results.json")
    with open(expected_path, "w") as f:
        json.dump(expected, f, indent=2)
        f.write("\n")
    print(f"wrote {expected_path}")

    wal_gz_path = os.path.join(args.output_dir, "wal.jsonl.gz")
    gzip_wal_dir(args.wal_dir, wal_gz_path)
    print(f"wrote {wal_gz_path}")

    # corpus_prose.jsonl.gz — the cleaned prose fed to the extractor for every consumed
    # article, the input fixture for comparing a different extraction model against this same
    # real corpus (#228). Written here (not just via --prose-only) so a normal capture run
    # produces all committed artifacts in one pass; costs one extra Wikipedia-fetch pass
    # (zero LLM/embedder calls) since the prose computed during ingest_corpus isn't retained
    # across the resume-skip branch (see build_corpus_prose).
    prose_records = build_corpus_prose(consumed_articles, args.wiki_delay)
    prose_path = os.path.join(args.output_dir, "corpus_prose.jsonl.gz")
    write_corpus_prose_gz(prose_records, prose_path)
    print(f"wrote {prose_path}")

    # Note the consumed/skipped article lists back on the manifest itself, so
    # corpus_manifest.json documents exactly what built the committed fixture (and can drive a
    # standalone --prose-only regeneration later) without needing expected_results.json open
    # alongside it. The unconsumed remainder stays in the manifest, ready to extend the corpus
    # in a future re-capture without re-curating article titles/revision IDs.
    manifest["last_capture"] = {
        "captured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "target_entities": args.target_entities,
        "consumed_article_count": len(consumed_articles),
        "manifest_article_count": total_manifest_size,
        "final_entity_count": expected["entity_count"],
        "final_relationship_count": expected["relationship_count"],
        "final_episode_count": expected["episode_count"],
        "consumed_articles": [
            {"title": a["title"], "revision_id": a["revision_id"]} for a in consumed_articles
        ],
        "skipped_articles": skipped_articles,
        "cleanup_version": CLEANUP_VERSION,
    }
    with open(args.manifest, "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
    print(f"annotated {args.manifest} with last_capture metadata")

    print(
        f"\nCapture complete: {len(consumed_articles)}/{total_manifest_size} articles consumed "
        f"({len(skipped_articles)} skipped), {expected['entity_count']} entities, "
        f"{expected['relationship_count']} relationships, {expected['episode_count']} episodes."
    )


if __name__ == "__main__":
    main()
