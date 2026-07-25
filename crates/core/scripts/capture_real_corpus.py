#!/usr/bin/env python3
"""
capture_real_corpus.py — capture the golden real-corpus WAL fixture for issue #217.

This is a one-time, offline, OUTSIDE-CI step (see specs/217-golden-real-corpus-wal/spec.md,
Assumptions). It drives a real, running `liminis-context-graph` service instance — configured
with the real `AnthropicExtractor` (via `ANTHROPIC_API_KEY`) and the real `OaiEmbedder` (the
local CoreML sidecar, `native/local-inference/`) — over its Unix socket, ingesting the pinned
Simple English Wikipedia corpus in `corpus_manifest.json`.

The capture is split into two explicit phases, run separately (`--stage-only` / `--ingest-only`)
or back-to-back (the default, with neither flag given):

  Phase 1 — stage (free, no service required, idempotent):
    Fetches and cleans every considered manifest article's wikitext into prose, skipping
    genuine stub articles (<200 chars of cleaned prose). Writes `corpus_prose.jsonl` — no
    LLM calls, no running service, no cost beyond Wikipedia fetches. Safe to re-run at will.

  Phase 2 — ingest (paid):
    Reads prose from the staged `corpus_prose.jsonl` — makes ZERO Wikipedia/network calls.
    Ingests staged articles in order via `knowledge_add_episode` (real extractor + embedder,
    over the service's Unix socket) until `knowledge_status` reports `entity_count >=
    --target-entities`, then captures `wal.jsonl` (the run's WAL files, copied verbatim into
    a `wal/` directory) and `expected_results.json`.

Splitting these phases matters because interleaving fetch/clean/ingest per article (the
original design) coupled a network dependency to an expensive LLM run: of six early capture
attempts, two died mid-run on a Wikipedia/infra fault after 20-36 articles of already-paid
extraction, discarding all of it. With staging done up front and persisted, the ingest phase
depends on nothing but the local service and cannot fail on a Wikipedia blip; interrupting and
re-running it does not refetch or re-clean anything (see #217).

The three fixture artifacts and what each is for:

  | Artifact                 | Purpose                                                    |
  |---------------------------|------------------------------------------------------------|
  | `corpus_manifest.json`   | provenance — pinned revisions, reproducibility             |
  | `corpus_prose.jsonl`     | re-extract the same inputs with a different model (#228)   |
  | `wal/*.jsonl`            | rebuild the graph deterministically, zero LLM calls         |
  | (future) LLM cassette    | replay one model's exact request/response exchange          |

`wal/*.jsonl` is *post-extraction*, so it cannot test a different extractor; a future cassette
records one model's exchanges, so it cannot test a different model either. Only the raw prose
that was fed in supports an apples-to-apples extraction-model comparison — and refetching from
Wikipedia at compare-time is not equivalent even with pinned revisions, because the prose is a
derived artifact of `wikitext_to_prose` (see `CLEANUP_VERSION` below): a future cleanup change
would silently change the compared input out from under a model-vs-model eval. `corpus_prose.jsonl`
is committed uncompressed and is exactly the staged input Phase 2 reads from — nothing is
regenerated or re-derived after staging, so the committed file and the ingest input are
byte-identical by construction.

Neither fixture artifact is gzip-compressed, and the WAL is committed as the run's individual
`.jsonl` files (original filenames preserved) rather than concatenated into one file: git
already compresses blobs for storage/transport, so gzip buys nothing on the wire while making
the fixture non-diffable and defeating delta compression across re-captures (a one-byte input
change would otherwise make the entire multi-MB blob a brand new object in history, forever).
Individual WAL files stay well under GitHub's per-file size warnings; a single concatenated file
would not.

Prerequisites (all one-time/local, never required in CI):
  1. `ANTHROPIC_API_KEY` set in the environment (real LLM extraction) — only for Phase 2.
  2. The local embedding sidecar reachable — either the default UDS socket
     (`/tmp/liminis-inference.sock`) or `LCG_EMBEDDING_URL` pointed at an HTTP endpoint — only
     for Phase 2. On a fresh checkout the sidecar compiles its `.mlpackage` fine but then fails
     with `offlineModeError("Repository not available locally")` fetching the
     `BAAI/bge-base-en-v1.5` tokenizer — it doesn't look in the repo by default even though the
     tokenizer ships in the embedding-assets release tarball. Point it there:
     `export LOCAL_INFERENCE_HF_CACHE=<repo>/resources/models/tokenizer`.
  3. The compiled `liminis-context-graph` binary (`cargo build --release`) — only for Phase 2.
  4. Network access to `simple.wikipedia.org` (to fetch pinned article revisions) — only for
     Phase 1.

Usage (full capture, staging then ingest in one command):
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
    git add crates/core/tests/fixtures/real_corpus_wal/wal/ \\
            crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl \\
            crates/core/tests/fixtures/real_corpus_wal/expected_results.json \\
            crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json
    git commit -m "test(corpus): capture golden real-corpus WAL fixture (#217)"

Usage (split — recommended for a long/expensive corpus, since Phase 1 is free and Phase 2
cannot fail on a Wikipedia blip once staging is done):

    # Phase 1 — free, no service, re-runnable at will
    python3 crates/core/scripts/capture_real_corpus.py --stage-only \\
        --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \\
        --output-dir crates/core/tests/fixtures/real_corpus_wal

    # Phase 2 — paid, reads corpus_prose.jsonl from --output-dir, zero network calls
    python3 crates/core/scripts/capture_real_corpus.py --ingest-only \\
        --socket /tmp/real_corpus_capture/service.sock \\
        --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \\
        --wal-dir /tmp/real_corpus_capture/wal \\
        --output-dir crates/core/tests/fixtures/real_corpus_wal

Pass --target-entities to override the default 1500-entity stopping point (e.g.
--target-entities 5000 for a deliberately larger fixture, or a smoke-test run with
--limit 20 --target-entities 50 to sanity-check the pipeline cheaply before a full capture).
--limit caps how many manifest articles Phase 1 stages (and therefore how many are available
for Phase 2 to ingest); it has no effect in --ingest-only (staging already happened).

The staging phase fails loudly (non-zero exit) on any article fetch error, and on the corpus as
a whole if too many attempted articles produce suspiciously short prose (see
SKIP_RATIO_THRESHOLD) — that's a systemic cleanup regression, not stub noise, and staging is the
free phase to catch it in, before any LLM spend. An individual short-prose article on its own is
just skipped and recorded in the staged file's header (`skipped_articles`) and later in
`expected_results.json`, since Simple English Wikipedia has many genuine stub articles. The
ingest phase fails loudly on any service/ingest error.

Resumable: at startup the ingest phase queries which episodes already exist for `--group-id`
and skips re-adding those articles. If a run aborts partway (a service crash, etc.), re-running
`--ingest-only` against the same (not fresh) DB/WAL dir picks up where it left off — with no
re-fetch or re-clean, since ingest never touches the network.

One-off backfill: the fixture actually committed for #217 was captured before this stage/ingest
split existed, so its `corpus_prose.jsonl` was derived after the fact directly from the already-
captured WAL's `Episodic` records (`--derive-prose-from-wal WAL_DIR`) rather than from a staged
file — zero network calls, and using the exact prose bytes that were fed to the extractor at
capture time (see `derive_prose_from_wal`). A future full re-capture doesn't need this: Phase 1's
staged `corpus_prose.jsonl` already *is* the ingest input.
"""

import argparse
import datetime
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Optional

WIKI_API = "https://simple.wikipedia.org/w/api.php"
USER_AGENT = "liminis-context-graph-fixture-capture/1.0 (https://github.com/verveguy/liminis-context-graph)"

# Bump this whenever `wikitext_to_prose` or any helper it calls (`strip_templates`,
# `strip_file_links`, the ref/link/heading regexes) changes behavior. It's recorded in
# `corpus_prose.jsonl`'s header record so a future extraction-model comparison (#228) can
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


# ── Phase 1: stage (fetch + clean, no service, no LLM) ─────────────────────────

# A too-short article aborts the whole staging run only once skipped articles exceed this
# share of attempted articles (and a minimum sample has been attempted, so one early stub
# doesn't trip the guard) — see #217: an isolated short article is normal stub-article
# noise on Simple English Wikipedia, not a cleanup regression. Checked during staging (free)
# rather than during ingest (paid), so a systemic cleanup regression is caught before any
# LLM spend.
SKIP_RATIO_THRESHOLD = 0.20
SKIP_RATIO_MIN_SAMPLE = 10


def stage_corpus(articles: list, wiki_delay: float) -> tuple:
    """Fetches and cleans each of `articles` into prose, in manifest order. Zero service/LLM
    calls — pure Wikipedia fetch + `wikitext_to_prose`. Returns `(staged, skipped)`:

      - `staged`: `[{title, revision_id, prose}, ...]` for articles with usable prose, in
        manifest order — this is exactly the committed `corpus_prose.jsonl` content and the
        input Phase 2 ingests from.
      - `skipped`: articles whose cleaned prose was suspiciously short (<200 chars), each as
        `{title, revision_id, prose_chars}` — genuine stub articles, not an error (Simple
        English Wikipedia has many).

    Raises if skipped articles exceed `SKIP_RATIO_THRESHOLD` of attempted articles once a
    minimum sample has been attempted — that's a systemic cleanup regression, not stub noise.
    """
    staged = []
    skipped = []
    for i, article in enumerate(articles, start=1):
        title = article["title"]
        revision_id = article["revision_id"]
        print(f"[{i}/{len(articles)}] fetching {title!r} (rev {revision_id})...", flush=True)
        wikitext = fetch_wikitext(revision_id)
        prose = wikitext_to_prose(wikitext)
        if len(prose) < 200:
            skipped.append(
                {"title": title, "revision_id": revision_id, "prose_chars": len(prose)}
            )
            attempted = len(staged) + len(skipped)
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

        staged.append({"title": title, "revision_id": revision_id, "prose": prose})
        time.sleep(wiki_delay)

    return staged, skipped


def script_git_sha() -> Optional[str]:
    """Best-effort git SHA of this script's own commit, for the corpus_prose.jsonl header.

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


def write_corpus_prose(staged: list, skipped: list, output_path: str) -> None:
    """Writes `staged` as plain (uncompressed) JSONL behind a header record, so
    `corpus_prose.jsonl` self-documents the cleanup version it was staged against (see
    CLEANUP_VERSION) and the articles skipped as stubs, without needing
    `expected_results.json` open alongside it.

    Uncompressed: git already compresses blobs for storage/transport, and a plain-text
    fixture is diffable/greppable and delta-compresses across re-captures — a gzip blob does
    neither (see module docstring).
    """
    header = {
        "_header": True,
        "cleanup_version": CLEANUP_VERSION,
        "script_git_sha": script_git_sha(),
        "record_count": len(staged),
        "skipped_articles": skipped,
    }
    with open(output_path, "w", encoding="utf-8") as out:
        out.write(json.dumps(header) + "\n")
        for record in staged:
            out.write(json.dumps(record) + "\n")


def read_corpus_prose(input_path: str) -> tuple:
    """Reads back a `corpus_prose.jsonl` written by `write_corpus_prose`. Returns
    `(staged, skipped)` — the exact staged records Phase 2 ingests from, and the skipped-stub
    list recorded at staging time (carried through into `expected_results.json`).
    """
    with open(input_path, encoding="utf-8") as f:
        lines = [json.loads(line) for line in f if line.strip()]
    if not lines or not lines[0].get("_header"):
        raise RuntimeError(f"{input_path} is missing its header record — not a valid staged corpus")
    header = lines[0]
    staged = lines[1:]
    if len(staged) != header.get("record_count"):
        raise RuntimeError(
            f"{input_path} header record_count={header.get('record_count')} does not match "
            f"{len(staged)} staged records found — file may be truncated/corrupted"
        )
    return staged, header.get("skipped_articles", [])


# ── Phase 2: ingest (paid, zero network calls) ──────────────────────────────────


def fetch_already_ingested_titles(client: ServiceClient, group_id: str) -> set:
    """Returns the episode `name`s already present for `group_id`.

    Lets a re-run of `--ingest-only` (pointed at a non-fresh DB/WAL dir left over from a
    prior aborted run) skip re-adding episodes it already paid for, instead of re-paying for
    the whole prefix on every retry (see #217).
    """
    result = client.call("knowledge_get_episodes", {"group_id": group_id, "last_n": 10000})
    return {ep.get("name") for ep in result.get("episodes", []) if ep.get("name")}


def ingest_from_staged(
    client: ServiceClient,
    staged: list,
    timestamps_by_key: dict,
    group_id: str,
    target_entities: int,
    already_ingested_titles: set = None,
) -> list:
    """Ingests `staged` records (from `corpus_prose.jsonl`) in order, calling
    `knowledge_add_episode` with the already-cleaned prose — no Wikipedia fetch, no wikitext
    cleanup, no network call beyond the local service socket. Polls `knowledge_status` after
    each newly added episode and stops as soon as `entity_count >= target_entities`.

    Returns `consumed`: staged records that contributed an episode to the graph, either just
    now or in a prior run (`already_ingested_titles`), in staged order — so the caller can
    record exactly what built the fixture.

    Raises if every staged record is consumed without reaching `target_entities` — that means
    the staged corpus needs more articles (re-run Phase 1 against a larger manifest slice),
    not a silently-undersized fixture.
    """
    already = already_ingested_titles or set()
    consumed = []
    entity_count = None
    for i, record in enumerate(staged, start=1):
        title = record["title"]
        revision_id = record["revision_id"]

        if title in already:
            print(
                f"[{i}/{len(staged)}] {title!r} already ingested (resuming) — skipping",
                flush=True,
            )
            consumed.append(record)
            continue

        ref_time = timestamps_by_key.get((title, revision_id))
        result = client.call(
            "knowledge_add_episode",
            {
                "name": title,
                "episode_body": record["prose"],
                "source": "text",
                "source_description": f"Simple English Wikipedia: {title} (rev {revision_id})",
                "reference_time": ref_time,
                "group_id": group_id,
            },
        )
        consumed.append(record)
        status = client.call("knowledge_status", {})
        entity_count = status["entity_count"]
        print(
            f"[{i}/{len(staged)}] {title!r} -> episode {result.get('episode_uuid')}, "
            f"entity_count={entity_count}",
            flush=True,
        )
        if entity_count >= target_entities:
            print(
                f"reached target_entities={target_entities} after {len(consumed)}/{len(staged)} "
                "staged articles — stopping ingest early",
                flush=True,
            )
            return consumed

    if entity_count is None:
        # Every staged article was already ingested in a prior run (pure resume) — check
        # status directly rather than assuming the target was reached.
        status = client.call("knowledge_status", {})
        entity_count = status["entity_count"]
        if entity_count >= target_entities:
            return consumed

    raise RuntimeError(
        f"exhausted all {len(staged)} staged articles without reaching "
        f"target_entities={target_entities} (last observed entity_count={entity_count}). "
        "Re-run --stage-only against a larger manifest slice (raise --limit or extend the "
        "manifest) and re-run --ingest-only."
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
    # handle_list_relationships (handlers.rs) returns {"facts": [...], "count": ...} — not
    # "edges". Reading the wrong key silently produced an empty relation_type_samples in the
    # #217 capture run (see the committed fixture's derive_relation_type_samples_from_wal
    # fallback), so a future capture must read "facts" here.
    edges = relationships.get("facts", [])
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


def copy_wal_dir(wal_dir: str, output_dir: str) -> list:
    """Copies the run's `.jsonl` WAL files verbatim (no concatenation, no compression) into
    `output_dir`, preserving original filenames. `WalReplayer` reads a directory and sorts
    files lexicographically (`replay.rs`), so this is exactly the layout the engine produces
    and consumes in production — closer to reality than a single synthetic concatenated file,
    and it keeps individual committed blobs small and delta-friendly across re-captures (see
    module docstring).
    """
    files = sorted(f for f in os.listdir(wal_dir) if f.endswith(".jsonl"))
    if not files:
        raise RuntimeError(f"no .jsonl WAL files found in {wal_dir}")
    os.makedirs(output_dir, exist_ok=True)
    for fname in files:
        shutil.copy2(os.path.join(wal_dir, fname), os.path.join(output_dir, fname))
    print(f"copied {len(files)} WAL files (original filenames preserved) -> {output_dir}")
    return files


_SOURCE_DESCRIPTION_REV_RE = re.compile(r"\(rev (\d+)\)\s*$")


def derive_prose_from_wal(wal_dir: str) -> list:
    """Derives staged-corpus-shaped records `[{title, revision_id, prose}, ...]` directly from
    an already-captured WAL directory's `CREATE (:Episodic {...})` records, with zero network
    calls — no Wikipedia refetch, no re-run of `wikitext_to_prose`.

    Each episode's `content` param is byte-identical to the prose that was actually fed to
    `knowledge_add_episode` (and therefore to the extractor) at capture time, so this is a more
    faithful source than re-deriving prose from Wikipedia through a cleanup function that may
    have changed since capture. `title` comes from the episode's `name` param (matches
    `capture_real_corpus.py`'s own `knowledge_add_episode` call, which passes the article title
    as `name`); `revision_id` is parsed back out of `source_description`
    (`"Simple English Wikipedia: {title} (rev {revision_id})"`, the exact string this script
    writes at ingest time).

    This is the one-off path used to backfill `corpus_prose.jsonl` for a WAL that was captured
    before the corpus-staging split existed (see #217) — a normal future capture's
    `corpus_prose.jsonl` comes from `write_corpus_prose` during Phase 1 instead, since that
    staged file *is* the ingest input, not something derived after the fact.

    Raises if an Episodic record's `source_description` doesn't match the expected
    `(rev <digits>)` suffix — better to fail loudly than to silently write a bogus/missing
    revision_id into the fixture.
    """
    records = []
    files = sorted(f for f in os.listdir(wal_dir) if f.endswith(".jsonl"))
    if not files:
        raise RuntimeError(f"no .jsonl WAL files found in {wal_dir}")
    for fname in files:
        with open(os.path.join(wal_dir, fname), encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                entry = json.loads(line)
                cypher = entry.get("cypher", "")
                if not cypher.startswith("CREATE (:Episodic {"):
                    continue
                params = entry["params"]
                title = params["name"]
                source_description = params.get("source_description", "")
                match = _SOURCE_DESCRIPTION_REV_RE.search(source_description)
                if not match:
                    raise RuntimeError(
                        f"Episodic {title!r} in {fname} has an unexpected "
                        f"source_description {source_description!r} — cannot recover revision_id"
                    )
                revision_id = int(match.group(1))
                records.append(
                    {"title": title, "revision_id": revision_id, "prose": params["content"]}
                )
    return records


def derive_relation_type_samples_from_wal(wal_dir: str, sample_size: int = 50) -> list:
    """Derives `relation_type_samples` (as `build_expected_results` would) directly from an
    already-captured WAL directory's `CREATE (:RelatesToNode_ {...})` records, with zero
    network/service calls.

    One-off backfill: `build_expected_results` read `knowledge_list_relationships`'s response
    under the wrong key (`"edges"` instead of the actual `"facts"`, see `handle_list_relationships`
    in `handlers.rs`), so the #217 capture's `expected_results.json` recorded an empty
    `relation_type_samples` despite the graph having 2,392 real relationships. Fixed for future
    captures (`build_expected_results` now reads `"facts"`); this function recovers the samples
    for the already-captured WAL without re-running the capture.
    """
    samples = []
    files = sorted(f for f in os.listdir(wal_dir) if f.endswith(".jsonl"))
    if not files:
        raise RuntimeError(f"no .jsonl WAL files found in {wal_dir}")
    for fname in files:
        if len(samples) >= sample_size:
            break
        with open(os.path.join(wal_dir, fname), encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                entry = json.loads(line)
                cypher = entry.get("cypher", "")
                if not cypher.startswith("CREATE (:RelatesToNode_ {"):
                    continue
                params = entry["params"]
                fact = params.get("fact")
                if not fact:
                    continue
                samples.append(
                    {
                        "uuid": params["uuid"],
                        "fact": fact,
                        "relation_type": params.get("relation_type"),
                    }
                )
                if len(samples) >= sample_size:
                    break
    return samples


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--socket", default=None, help="Path to the running service's Unix socket (required for ingest)"
    )
    parser.add_argument(
        "--manifest", default=None, help="Path to corpus_manifest.json (not needed with --derive-prose-from-wal)"
    )
    parser.add_argument(
        "--wal-dir", default=None, help="WAL directory the running service writes to (required for ingest)"
    )
    parser.add_argument("--output-dir", required=True, help="real_corpus_wal/ fixture directory")
    parser.add_argument("--group-id", default="apollo_program", help="group_id used for all episodes")
    parser.add_argument(
        "--wiki-delay",
        type=float,
        default=1.0,
        help="seconds to sleep between Wikipedia API fetches during staging (rate-limit friendliness)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="cap the number of manifest articles staged (for a quick smoke-test run); no effect with --ingest-only",
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
        "--stage-only",
        action="store_true",
        help=(
            "Run Phase 1 only: fetch + clean manifest articles into corpus_prose.jsonl under "
            "--output-dir. No service, no ANTHROPIC_API_KEY, no embedder, no --socket/--wal-dir "
            "needed — costs only Wikipedia fetches."
        ),
    )
    parser.add_argument(
        "--ingest-only",
        action="store_true",
        help=(
            "Run Phase 2 only: ingest from the corpus_prose.jsonl already staged under "
            "--output-dir (requires a prior --stage-only or combined run). Requires --socket "
            "and --wal-dir. Makes zero network calls."
        ),
    )
    parser.add_argument(
        "--derive-prose-from-wal",
        default=None,
        metavar="WAL_DIR",
        help=(
            "One-off backfill: derive corpus_prose.jsonl under --output-dir directly from an "
            "already-captured WAL directory's Episodic records (content/name/source_description "
            "params) instead of Wikipedia — zero network calls, and immune to any cleanup-code "
            "drift since the WAL was captured. For a WAL captured before the stage/ingest split "
            "existed. No --socket/--manifest/--wal-dir needed."
        ),
    )
    parser.add_argument(
        "--backfill-relation-samples-from-wal",
        default=None,
        metavar="WAL_DIR",
        help=(
            "One-off backfill: recompute relation_type_samples in the expected_results.json "
            "already under --output-dir directly from an already-captured WAL directory's "
            "RelatesToNode_ records, and rewrite the file in place — zero network/service calls. "
            "Fixes captures made before build_expected_results read knowledge_list_relationships' "
            "response under the correct 'facts' key (see derive_relation_type_samples_from_wal)."
        ),
    )
    args = parser.parse_args()

    modes = [
        args.stage_only,
        args.ingest_only,
        bool(args.derive_prose_from_wal),
        bool(args.backfill_relation_samples_from_wal),
    ]
    if sum(modes) > 1:
        raise SystemExit(
            "--stage-only, --ingest-only, --derive-prose-from-wal, and "
            "--backfill-relation-samples-from-wal are mutually exclusive"
        )

    if args.derive_prose_from_wal:
        os.makedirs(args.output_dir, exist_ok=True)
        records = derive_prose_from_wal(args.derive_prose_from_wal)
        prose_path = os.path.join(args.output_dir, "corpus_prose.jsonl")
        write_corpus_prose(records, [], prose_path)
        print(
            f"wrote {prose_path} ({len(records)} records derived from "
            f"{args.derive_prose_from_wal}, zero network calls)"
        )
        return

    if args.backfill_relation_samples_from_wal:
        expected_path = os.path.join(args.output_dir, "expected_results.json")
        with open(expected_path) as f:
            expected = json.load(f)
        samples = derive_relation_type_samples_from_wal(args.backfill_relation_samples_from_wal)
        expected["relation_type_samples"] = samples
        with open(expected_path, "w") as f:
            json.dump(expected, f, indent=2)
            f.write("\n")
        print(f"backfilled {len(samples)} relation_type_samples into {expected_path}")
        return

    if not args.manifest:
        raise SystemExit("--manifest is required unless --derive-prose-from-wal is given")

    with open(args.manifest) as f:
        manifest = json.load(f)

    prose_path = os.path.join(args.output_dir, "corpus_prose.jsonl")

    if args.stage_only:
        articles = manifest["articles"]
        if args.limit:
            articles = articles[: args.limit]
        os.makedirs(args.output_dir, exist_ok=True)
        staged, skipped = stage_corpus(articles, args.wiki_delay)
        write_corpus_prose(staged, skipped, prose_path)
        print(
            f"wrote {prose_path} ({len(staged)} staged, {len(skipped)} skipped as stubs, "
            f"cleanup_version={CLEANUP_VERSION})"
        )
        return

    if args.ingest_only:
        if not args.socket or not args.wal_dir:
            raise SystemExit("--ingest-only requires --socket and --wal-dir")
        staged, skipped_articles = read_corpus_prose(prose_path)
        _run_ingest(args, manifest, staged, skipped_articles)
        return

    # Combined default: stage, then ingest, in one command.
    if not args.socket or not args.wal_dir:
        raise SystemExit("--socket and --wal-dir are required unless --stage-only is given")
    articles = manifest["articles"]
    if args.limit:
        articles = articles[: args.limit]
    os.makedirs(args.output_dir, exist_ok=True)
    staged, skipped_articles = stage_corpus(articles, args.wiki_delay)
    write_corpus_prose(staged, skipped_articles, prose_path)
    print(
        f"wrote {prose_path} ({len(staged)} staged, {len(skipped_articles)} skipped as stubs)"
    )
    _run_ingest(args, manifest, staged, skipped_articles)


def _run_ingest(args, manifest: dict, staged: list, skipped_articles: list) -> None:
    """Shared Phase-2 driver for both the combined default path and --ingest-only: ingests
    `staged` records, builds `expected_results.json`, copies the WAL, and annotates the
    manifest. Makes zero Wikipedia/network calls — `staged` already carries the cleaned prose.
    """
    timestamps_by_key = {
        (a["title"], a["revision_id"]): a["revision_timestamp"] for a in manifest["articles"]
    }
    total_manifest_size = len(manifest["articles"])

    start = time.monotonic()
    client = ServiceClient(args.socket)
    try:
        already_ingested_titles = fetch_already_ingested_titles(client, args.group_id)
        if already_ingested_titles:
            print(
                f"resuming: {len(already_ingested_titles)} episode(s) already present for "
                f"group_id={args.group_id!r} — matching staged articles will be skipped",
                flush=True,
            )
        consumed_articles = ingest_from_staged(
            client,
            staged,
            timestamps_by_key,
            args.group_id,
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

    os.makedirs(args.output_dir, exist_ok=True)
    expected_path = os.path.join(args.output_dir, "expected_results.json")
    with open(expected_path, "w") as f:
        json.dump(expected, f, indent=2)
        f.write("\n")
    print(f"wrote {expected_path}")

    wal_output_dir = os.path.join(args.output_dir, "wal")
    copy_wal_dir(args.wal_dir, wal_output_dir)

    # Note the consumed/skipped article lists back on the manifest itself, so
    # corpus_manifest.json documents exactly what built the committed fixture, without needing
    # expected_results.json open alongside it. The unconsumed remainder stays in the manifest,
    # ready to extend the corpus in a future re-capture without re-curating article
    # titles/revision IDs.
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
