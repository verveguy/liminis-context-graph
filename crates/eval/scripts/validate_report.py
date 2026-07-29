#!/usr/bin/env python3
"""Post-run validation for an lcg-eval report and the cassettes it was scored from.

Usage: validate_report.py REPORT BASELINE_CASSETTE CANDIDATE_CASSETTE QWEN_CASSETTE

Checks each cassette's record count against the report's OWN accounting
(``chunks_run - errors``) rather than against a completeness threshold.

The two are not interchangeable. Picking which legs to replay happens *before* any report
exists, so a proportional threshold is the only tool available there. Post-run the report
states the answer exactly, and using a threshold instead conflates two different things:

* a **truncated** capture — the process died partway, so the report was scored against
  partial data and is invalid;
* a capture where some chunks **legitimately errored** — normal, already accounted for in
  ``error_rate``, and not grounds for discarding anything.

The threshold version failed on its first live run for precisely this reason: at
``LIMIT=3`` one malformed qwen output is a 33% error rate, so a perfectly good report was
declared INVALID. A proportion cannot tell one error from truncation at small N.

Lives in its own file rather than a heredoc inside the shell script so it can be tested
directly, and so quoting inside it is not at the mercy of the surrounding shell.
"""

import json
import sys

LEGS = ("baseline", "candidate", "qwen")
HIGH_ERROR_RATE = 0.10


def records(path: str) -> int:
    with open(path) as f:
        return sum(1 for line in f if line.strip())


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        print(__doc__, file=sys.stderr)
        return 2

    report_path = argv[1]
    cassettes = dict(zip(LEGS, argv[2:5]))

    try:
        report = json.load(open(report_path))
    except Exception as e:  # a missing or malformed report is itself a failure
        print(f"ERROR: cannot read report {report_path}: {e}", file=sys.stderr)
        return 1

    by_name = {c["backend_name"]: c for c in report.get("candidates", [])}

    problems = []
    notes = []
    for leg, path in cassettes.items():
        c = by_name.get(leg)
        if c is None:
            problems.append(f"{leg}: absent from the report entirely")
            continue
        try:
            actual = records(path)
        except OSError as e:
            problems.append(f"{leg}: cannot read {path}: {e}")
            continue

        expected = c["chunks_run"] - c["errors"]
        if actual != expected:
            problems.append(
                f"{leg}: {path} holds {actual} records but the report accounts for "
                f"{expected} ({c['chunks_run']} run - {c['errors']} errored). The capture "
                "was truncated, so this report was scored against partial data."
            )
        elif c["errors"] and c["error_rate"] > HIGH_ERROR_RATE:
            # Loud but not fatal: a high error rate says something about the model, not
            # about whether the run completed.
            notes.append(
                f"{leg}: {c['errors']}/{c['chunks_run']} chunks errored "
                f"({c['error_rate'] * 100:.0f}%) — accounted for, but read that axis carefully"
            )

    for n in notes:
        print(f"    NOTE {n}", file=sys.stderr)

    if problems:
        print(f"ERROR: {report_path} is INVALID:", file=sys.stderr)
        for p in problems:
            print(f"       {p}", file=sys.stderr)
        return 1

    print(f"    validated: every cassette matches {report_path}'s accounting")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
