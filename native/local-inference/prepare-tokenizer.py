#!/usr/bin/env python3
"""
Stage HuggingFace tokenizer files for offline swift-transformers loading.

Output: <repo_root>/resources/models/tokenizer/models/BAAI/bge-base-en-v1.5/

== Offline loader contract (swift-transformers HubApi, pinned at v0.1.24) ==
The loader walks the TOP-LEVEL visible files in:
  <downloadBase>/models/<org>/<model>/
For EACH file found, it requires a metadata marker at:
  <modelDir>/.cache/huggingface/download/<filename>.metadata
A missing marker throws offlineModeError("Metadata not available for <filename>").
A malformed marker is deleted on detection, causing the same error on next launch.

Marker format (3 lines, trailing newline):
  <commit-hash>   -- 40-char hex; real value from snapshot preferred (FR-004)
  <etag>          -- any string; stub is acceptable (loader ignores in offline mode)
  <timestamp>     -- Unix epoch as float string (must parse as Double)

REQUIRED FILES (all must be present at the top level):
  tokenizer.json          -- fast-tokenizer vocabulary and merge rules
  tokenizer_config.json   -- tokenizer class and special-token config
  vocab.txt               -- WordPiece vocabulary (BertTokenizer fallback)
  special_tokens_map.json -- special-token definitions
  config.json             -- BERT model config (hidden_size, num_attention_heads, ...)
                             required by AutoTokenizer.from() when loading offline

Removing or renaming any of these files without updating this docstring and
check-embedding-assets.sh will break the offline tokenizer load at sidecar startup.
==

Uses huggingface_hub.snapshot_download to get a deterministic snapshot path
rather than scraping the cache directory, so we are insulated from changes in
HF cache naming conventions.

Usage:
    uv run prepare-tokenizer.py [--model BAAI/bge-base-en-v1.5] [--output <dir>]
                                [--revision <commit-hash>]
"""
# /// script
# requires-python = ">=3.11,<3.13"
# dependencies = [
#     "huggingface_hub>=0.25,<1.0",
# ]
# ///

from __future__ import annotations

import argparse
import os
import shutil
import sys
import time
from pathlib import Path

# Pinned HuggingFace commit for BAAI/bge-base-en-v1.5 used by test fixtures.
# Update this constant (and run refresh-test-fixtures.sh) when the upstream model changes.
PINNED_BGE_REVISION = "a5beb1e3e68b9ab74eb54cfd186867f64f240e1a"

# All files the swift-transformers offline loader requires at the top level.
# See the docstring above for the loader contract -- edit this list only if
# you also update the contract documentation and check-embedding-assets.sh.
TOKENIZER_FILES = [
    "tokenizer.json",
    "tokenizer_config.json",
    "vocab.txt",
    "special_tokens_map.json",
    "config.json",
]

# Stub etag value -- the offline loader does not validate etag content for
# non-LFS files; any non-empty string is accepted.
_STUB_ETAG = "prepare-tokenizer-stub-etag"


def default_output(model: str) -> Path:
    # Mirrors HubApi(downloadBase:) expectations: <root>/models/<org>/<model>/
    here = Path(__file__).resolve().parent  # native/local-inference
    repo_root = here.parent.parent          # liminis-context-graph repo root
    return repo_root / "resources" / "models" / "tokenizer" / "models" / model


def write_metadata_marker(marker_path: Path, commit_hash: str) -> None:
    """Write a swift-transformers-compatible offline metadata marker."""
    marker_path.parent.mkdir(parents=True, exist_ok=True)
    timestamp = str(time.time())
    marker_path.write_text(f"{commit_hash}\n{_STUB_ETAG}\n{timestamp}\n")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Stage HuggingFace tokenizer files for offline swift-transformers loading"
    )
    parser.add_argument(
        "--model",
        default="BAAI/bge-base-en-v1.5",
        help="HuggingFace repo id",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Destination directory (defaults to <repo_root>/resources/models/tokenizer/models/<model>/)",
    )
    parser.add_argument(
        "--revision",
        default=PINNED_BGE_REVISION,
        help="HuggingFace commit hash to pin (defaults to PINNED_BGE_REVISION constant)",
    )
    args = parser.parse_args()

    output = Path(args.output) if args.output else default_output(args.model)

    from huggingface_hub import snapshot_download

    print(f"Downloading {args.model} snapshot (tokenizer files only, revision={args.revision[:8]}...)...")
    snapshot_dir = snapshot_download(
        repo_id=args.model,
        revision=args.revision,
        allow_patterns=TOKENIZER_FILES,
    )
    snapshot_path = Path(snapshot_dir)

    # The last path component of a HuggingFace snapshot IS the commit hash.
    commit_hash = snapshot_path.name

    missing = [name for name in TOKENIZER_FILES if not (snapshot_path / name).exists()]
    if missing:
        print(f"ERROR: snapshot missing required files: {missing}", file=sys.stderr)
        return 1

    # Write to a temporary sibling directory, then atomically rename it over
    # the output directory.  This prevents a partial output on failure (FR-007,
    # FR-008).
    output.parent.mkdir(parents=True, exist_ok=True)
    tmp_output = output.parent / f".tmp_{output.name}"
    if tmp_output.exists():
        shutil.rmtree(tmp_output)
    tmp_output.mkdir(parents=True)

    try:
        # Copy tokenizer files to the temp directory
        for name in TOKENIZER_FILES:
            src = snapshot_path / name
            dest = tmp_output / name
            shutil.copyfile(src, dest)
            size = os.path.getsize(dest)
            print(f"  {name}  ({size:,} bytes)")

        # Write a .metadata marker for every staged file.
        # The swift-transformers offline loader walks the top-level visible
        # files and requires a marker for each one (see docstring contract).
        metadata_dir = tmp_output / ".cache" / "huggingface" / "download"
        for name in TOKENIZER_FILES:
            write_metadata_marker(metadata_dir / f"{name}.metadata", commit_hash)
            print(f"  .cache/huggingface/download/{name}.metadata  (commit {commit_hash[:8]}...)")

        # Atomic directory swap: rename old output aside first so the
        # destination is never absent between the delete and rename.
        old_output = output.parent / f".old_{output.name}"
        if output.exists():
            output.rename(old_output)
        tmp_output.rename(output)
        if old_output.exists():
            shutil.rmtree(old_output, ignore_errors=True)

    except Exception:
        shutil.rmtree(tmp_output, ignore_errors=True)
        raise

    print(f"Done. Tokenizer files staged at: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
