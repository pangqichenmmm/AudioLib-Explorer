# Directory search timeout investigation — 2026-11-05

Base: `8b53f7b` on `feat/search-metadata-v2`. The format remains
`ale-s9-cld1-t3-m2-p1-a1-sd1`; tokenization, directory tables and cursor revision
are unchanged. This report concerns search execution and pagination, not a
schema migration or a deployment.

## Reproduction

Read-only GET requests to the beta instance running the base commit used
`/api/v1/search`, `limit=200`, and the three queries below. The library reported
909,342 audio records, 20,248 folders, zero missing records, and schema 9.
These are individual HTTP wall-clock observations, not p95 measurements.

| Query | main status / ms | all status / ms | path status / ms |
| --- | ---: | ---: | ---: |
| Borg Sound | 200 / 372.64 | 200 / 2533.64 | 408 / 30009.24 |
| Rain Indoor Ambiences | 200 / 1494.16 | 408 / 30007.20 | 408 / 30010.04 |
| Car Collection | 200 / 4173.32 | 200 / 3116.90 | 408 / 30008.43 |

The beta service was neither upgraded nor restarted. A complete snapshot was
downloaded with GET for isolated local measurement; no production endpoint or
persistent client library was used.

## Cause and query plans

The old mixed tier expands directory runs and repeatedly executes
`SELECT 1 FROM audio_search WHERE audio_search MATCH ?1 AND rowid=?2` for
individual candidates. It also expands directories already satisfying all
path terms, only to discard their rows from the mixed tier. `scope=path` then
unnecessarily checks full-main matches for those all-path rows again.

The real fixture has approximately 161,135 partial-path candidates for
`Borg Sound`, 13,474 for `Rain Indoor Ambiences`, and 207,379 for `Car Collection`.
Skipping only complete-path directories therefore does not address the main
source of work.

The corrected mixed tier records each directory's missing terms, then merges
rowid-ordered main postings in batches of at most 256 IDs per needed term.
It requires every missing term and excludes full-main matches through a separate
advancing stream. Complete-path directories bypass the mixed tier. Path-only
complete matches never probe main; all-scope complete matches use the advancing
full-main stream for deduplication. The search stops after the requested prefix
and lookahead, with bounded posting buffers rather than full posting lists.

Actual `EXPLAIN QUERY PLAN` and `EXPLAIN` output from SQLite 3.54.1 on the
downloaded snapshot includes:

```text
segment MATCH: SCAN audio_dir_search VIRTUAL TABLE INDEX 0:M2
old row probe: SCAN audio_search VIRTUAL TABLE INDEX 0:=M12
old row probe VDBE: VFilter ... =M12
bounded posting stream: SCAN audio_search VIRTUAL TABLE INDEX 64:M12><
bounded posting VDBE: VFilter ... M12><
visibility: SEARCH audio_files USING INTEGER PRIMARY KEY (rowid=?)
owned run expansion: SEARCH audio_files USING INDEX idx_audio_files_folder (folder_id=? AND rowid>? AND rowid<?)
```

The virtual-table `SCAN` label is an FTS posting scan, not proof of a full
`audio_files` scan. The rowid constraint is present in the plan. The problem is
the number of repeated MATCH executions in the application loop; changing an
index or increasing the HTTP deadline would not address that work.

Reproduce the SQL-plan evidence with `search-diagnose <snapshot> --path
"Borg Sound"` (also run the other two queries). This mode executes EXPLAIN only.
Its quoted raw MATCH bindings are representative planner inputs, not the
production Unicode/fragment compiler. Full opcode output stays in the local
diagnostic evidence rather than this report.

Sparse run expansion also requires the recorded `folder_id`. A NULL-folder row
inside an ID gap cannot become a path hit merely because its ID lies in the run;
its independent main-field hit remains eligible for the main tier.

## Count and continuation contract

Nonempty directory searches retain bounded `limit + 1` lookahead. Without a
cursor, `total_count` counts the observed prefix, including skipped hits and
lookahead; it is a lower bound until exhaustion. An offset past the end returns
the actual observed total, not the requested offset. With a cursor, the count
is local to the suffix after that cursor and is not a global result total.
Use `next_cursor` to continue; internal `has_more` is not a new response field.

Main search, empty-query browsing and `all` falling back to main retain their
existing exact counts. No DTO fields or JSON-schema validation rules change.
The desktop's existing numeric display is unchanged; it must not be treated as
an exact directory-search total. See [API pagination](../../doc/api/README.md#5-分页).

## Measurement method

`search-b-bench provider <complete-snapshot>` measures the production server
provider, including result hydration. It first copies the immutable snapshot
into an owned temporary library and opens the server provider there. Copying
and startup are outside timing. The source snapshot is unchanged. The existing
prototype benchmark modes do not exercise `dir_search.rs` and are not evidence
for this fix.

Use `ROUNDS=5`, `WARM=60`: each round has three warmups and 60 timed searches,
then report the median of the five round p95 values for each query and scope.
The cache condition is a warm provider and warm OS cache, not OS-cold. This
does not measure HTTP, UI latency or the remote Mac's performance.

Local host: Windows x64, AMD Ryzen 7 9800X3D (8 cores / 16 threads), approximately
93.5 GiB RAM, Samsung SSD 860 QVO SATA / local NTFS fixture storage,
Rust 1.97.1 MSVC, optimized release helpers. Serial workspace validation ran
alongside the measurements; these are not measurements on an otherwise idle
machine. p95 uses nearest rank (the 57th sorted sample out of 60).

## Results

All 2,835 searches (including warmups) completed without a search error or
result/count drift. Times below are milliseconds; main uses its normal warm
ranking cache (60 cache hits per round), whereas all/path report zero ranking
cache hits. This difference is another reason not to interpret these numbers
as a schema-8 comparison.

| Query | Scope | Five round p95 values | Median p95 | Returned / count / next cursor |
| --- | --- | --- | ---: | --- |
| Borg Sound | main | 19.06, 24.86, 24.04, 23.42, 23.69 | 23.69 | 200 / 2073 / no |
| Borg Sound | all | 350.51, 363.49, 360.27, 359.60, 350.94 | 359.60 | 200 / 201 / yes |
| Borg Sound | path | 549.72, 555.75, 551.29, 542.97, 531.81 | 549.72 | 9 / 9 / no |
| Rain Indoor Ambiences | main | 0.49, 0.45, 0.56, 0.51, 0.62 | 0.51 | 8 / 8 / no |
| Rain Indoor Ambiences | all | 340.52, 340.14, 339.66, 339.07, 335.45 | 339.66 | 9 / 9 / no |
| Rain Indoor Ambiences | path | 156.48, 152.44, 156.31, 155.87, 156.48 | 156.31 | 4 / 4 / no |
| Car Collection | main | 12.70, 12.65, 12.73, 12.56, 8.65 | 12.65 | 200 / 18730 / no |
| Car Collection | all | 781.92, 769.09, 773.37, 772.11, 768.64 | 772.11 | 200 / 201 / yes |
| Car Collection | path | 214.02, 220.70, 221.26, 218.70, 217.20 | 218.70 | 200 / 201 / yes |

The path timeouts were reproduced on the unchanged beta and the corrected
local provider completed these queries. This is not a same-host before/after
speedup measurement or acceptance of a deployed beta fix. Borg/path and
Car/all still exceed 500 ms in this run.

No matching schema-8 fixture is available in this worktree. Same-snapshot main
search is only a surrogate comparison; it cannot establish the **s9 ≤ s8 × 1.2**
acceptance gate. The separate product target remains **P95 latency <500ms for
1M+ records**; this 909,342-record warm run alone cannot establish that target.

## Validation

Local Rust checks use MSVC 1.97.1, owned NTFS temporary data and serial test
threads. No application deployment or installation was performed.

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --locked --features
  audiolib-db/test-hooks,audiolib-server/test-hooks -- -D warnings`: passed after
  the streaming rewrite. Strict all-targets Clippy for `audiolib-db` with
  `test-hooks` passed again after the final sparse-run ownership filter.
- Final-source release regressions: 14 directory-search unit tests, 5 database
  migration integration tests, 1 server path-search API test and 5 server
  migration tests passed. They cover posting refills, tier/cursor ordering,
  observed counts, sparse ownership, bounded VM work, full-path main-FTS
  avoidance, migration cache refresh and the retained admin access policy.
- `cargo test --workspace --all-targets --locked --features
  audiolib-db/test-hooks,audiolib-server/test-hooks -- --test-threads=1`:
  completed successfully across 94 Cargo targets, with 1,973 passed, zero
  failed and 198 ignored declarations. Two nested child-harness summaries are
  excluded from these totals. The ignored declarations include 148 isolated
  child helpers; opt-in migration/performance, hardware/UI and external-fixture
  acceptance remain distinct from the tests executed here.
- The broad workspace run began before the final search refinements. Unchanged
  coverage is reused; the affected directory search and direct consumers were
  rerun on the final source as listed above. This is not a claim that every
  broad test was recompiled after the final narrow filter.
- The benchmark rejects a nonempty source WAL without changing the source or
  WAL bytes. The accepted source files and measured helper identities were
  checked against private hash receipts.
- Both JSON schemas parse and retain their validation shape. `git diff --check`
  and the added-line private-address/path/credential scan passed.
