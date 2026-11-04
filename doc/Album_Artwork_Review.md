# Album artwork review

Reviewed `feat/album-artwork` at `5687c4b` against `beta` at `b1c07ac` on
2026-10-27. The review branch includes beta's subsequent local-library and
Waveform work. No original library or source WAV was modified during review.

## Corrected findings

- **P1 — Album selection did not filter results.** Navigation rebuilt the filter
  struct without its album value. Preserve the selected value, reset it with the
  other filters, and restore it with saved search tasks.
- **P1 — The additive schema upgrade bypassed migration leases.** Acquire the
  scan, administration and reader leases, validate the source, and apply the
  cancellable backfill in one transaction. Include the full signed ID range.
- **P1 — A migration could publish schema 7 without its album column.** Create
  and fill the derived column in the owned candidate before publication. Keep
  authoritative Metadata and annotations in the migration business digest.
- **P1 — Schema 6 snapshot annotation refresh used legacy SQL columns.** Select
  the update path from the actual storage layout. Read older snapshots' album
  values from Metadata without altering the snapshot.
- **P2 — Sidebar results used previous search conditions and survived source
  changes.** Use the current request and the existing cancellable facet worker;
  invalidate publications and clear presentation when switching context.
- **P2 — Cover extraction delayed playback and decoded unrestricted images.**
  Move preview work to a coalescing worker, skip remote stream previews, validate
  source paths, limit encoded and decoded image sizes, and retain display-sized
  pixels only.
- **P2 — Cached covers were shared forever by album name.** Include library,
  canonical source path, source length and modification time in the cache key.
- **P2 — Valid ID3 artwork was skipped or corrupted.** Permit bounded 2–8 MiB
  covers while seeking past binary frames during ordinary Metadata scans.
  Handle supported unsynchronisation, grouping and data-length flags, and skip
  unsupported frames. Increment the Metadata extractor version for rescanning.

## Validation scope

Regression coverage includes ID3 sizes/flags, bounded image decoding, cache source
identity, real Controller filtering/task restoration/stale publications, upgrade
leases/cancellation, v5 migration, real v6 storage layout, old snapshot searches,
and annotation preservation on rescan. Validation commands and generated logs
are local to `.rust-validation/album-review/`.

The local acceptance sequence uses strict workspace Clippy and workspace tests,
the verified ASIO SDK release build, portable packaging/startup checks, library
management and cross-process data-safety validators, and synthetic album CLI/UI
inspection. Installation targets the existing repository `local_release/`, with
package file SHA256 and isolated startup verification.

Real BOOM P-51 WAV fixtures are unavailable in this checkout. Real fixture
acceptance, macOS/Linux behavior, ASIO hardware output and physical playback
latency are unverified. No release tag or GitHub Release is part of this merge.
