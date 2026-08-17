---
layout: default
title: Release Process
---

# Release process

This is the maintainer procedure for verifying CI status before cutting a release. It
exists because reading a job's pass/fail *conclusion* alone is not a trustworthy signal
in this repository: issue #430 found that a `2>&1 | tee <log>` pattern in six `ci.yml`
jobs (the required `test (ubuntu-latest)` gate plus all five real-corpus e2e jobs) and
three `bench.yml` steps ran under GitHub Actions' implicit default shell, which has no
`pipefail` — so each step's exit status was `tee`'s, not the piped test/bench command's.
Three consecutive `main` runs were found with `test result: FAILED` in the job log while
reporting a passing conclusion, and the masked gate let a real regression (#428) ship in
releases 0.13.0, 0.13.1, and 0.13.2, each "verified" only by reading job conclusions. See
[ADR-0430](adr/0430-ci-tee-pipefail.md) for the fix.

The fix (a workflow-level `shell: bash` default, restoring `pipefail`) makes conclusions
trustworthy going forward, but grepping the log is still the documented step here as
defense in depth: the whole point of #430 is that "the conclusion looked right" was
already true of the runs that turned out to be broken.

## Before cutting a release

For the release commit's CI run on `main`, check **both** of the following — a passing
conclusion alone is not sufficient:

1. **Job conclusions.** `test (ubuntu-latest)` and all five real-corpus e2e jobs
   (`real_corpus_e2e`, `mcp_real_corpus_e2e`, `mcp_real_corpus_mutation_e2e`,
   `mcp_real_corpus_admin_data_e2e`, `mcp_real_corpus_admin_lifecycle_e2e`) report
   success:

   ```bash
   gh run list --branch main --limit 1 --json databaseId,conclusion,name
   ```

2. **Log grep for the actual test result.** For each of those six jobs' run, confirm the
   log contains no `test result: FAILED` line:

   ```bash
   gh run view <run-id> --log | grep -a "test result: FAILED"
   ```

   No output means no failing test was masked. Any match — even alongside a "success"
   conclusion — means do not cut the release; investigate first.

Do not treat step 1 alone as sufficient evidence that "full e2e passed." Step 2 is the
one that actually verifies it.

## Related

- [ADR-0430](adr/0430-ci-tee-pipefail.md) — the `tee`/`pipefail` defect this process
  works around, and the workflow-level fix
- `.github/workflows/ci.yml` — the required gate and five e2e jobs
- #428 — the regression that shipped behind these jobs while conclusion-only
  verification was in use
