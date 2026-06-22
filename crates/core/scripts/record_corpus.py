#!/usr/bin/env python3
"""
record_corpus.py — capture IPC parity fixtures from the upstream Python graphiti-core service.

Usage:
    python scripts/record_corpus.py \
        --socket /tmp/lcg/service.sock \
        --output tests/fixtures/ipc_corpus/ \
        --golden tests/fixtures/golden_queries.json

Prerequisites:
    1. Python graphiti_service.py running: LCG_DB_PATH=/tmp/baseline.db python graphiti_service.py
    2. The baseline DB should be freshly populated with a representative episode set.

After capture:
    - Copy tests/fixtures/baseline_db/liminis.db from the running service's DB path.
    - Commit all files in tests/fixtures/ to lock down the parity baseline.
    - Set PARITY_GOLDEN=1 in CI to enable the rank-correlation test (SC-002).

NOTE: This script is not yet implemented. See issue #2 for context.
When the Python service is available, implement the socket I/O loop that:
  1. Sends each method's request JSON (newline-delimited).
  2. Reads the response.
  3. Writes { "request": ..., "response": ... } to the output directory.
"""

import argparse
import json
import os
import socket
import sys


def send_request(sock: socket.socket, req: dict) -> dict:
    line = json.dumps(req) + "\n"
    sock.sendall(line.encode())
    buf = b""
    while b"\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("socket closed before response")
        buf += chunk
    return json.loads(buf.split(b"\n")[0])


FIXTURES = [
    {
        "filename": "build_indices_01.json",
        "request": {"jsonrpc": "2.0", "id": 1, "method": "knowledge_build_indices", "params": {}},
    },
    {
        "filename": "add_episode_01.json",
        "request": {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "knowledge_add_episode",
            "params": {
                "name": "team-intro",
                "episode_body": "Alice works at Acme Corp as a software engineer.",
                "source": "test",
                "source_description": "parity fixture",
                "reference_time": "2026-01-01T00:00:00Z",
                "group_id": "parity_group",
            },
        },
    },
    # Add remaining methods here as needed.
]


def main() -> None:
    parser = argparse.ArgumentParser(description="Record IPC parity fixtures")
    parser.add_argument("--socket", required=True, help="Path to the Unix socket")
    parser.add_argument("--output", required=True, help="Output directory for fixture JSON files")
    parser.add_argument("--golden", required=True, help="Path to golden_queries.json")
    args = parser.parse_args()

    os.makedirs(args.output, exist_ok=True)

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.connect(args.socket)
        print(f"Connected to {args.socket}")

        for fixture in FIXTURES:
            resp = send_request(sock, fixture["request"])
            out = {"request": fixture["request"], "response": resp}
            path = os.path.join(args.output, fixture["filename"])
            with open(path, "w") as f:
                json.dump(out, f, indent=2)
            print(f"Wrote {path}")

    print("Corpus capture complete.")


if __name__ == "__main__":
    main()
