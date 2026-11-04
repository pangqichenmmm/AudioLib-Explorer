# Codex handoff — 2026-10-27

> Historical handoff written before the completed investigation and authorized
> deployment. See [playback evidence](Playback_Fix_20261027.md) and the
> [latest validation report](../doc/Rust_Validation.md). The hypotheses, pending
> work and restrictions below describe the handoff state.

Shared handoff for `handoff/codex-20261027`. This file currently contains only the Debug section. Other tracks should append their own sections. Do not merge this branch to `beta`.

The branch already contains `d807cad` ("Add phase-1 server album artwork endpoint") from another track. That commit is not part of this playback investigation. Do not extend it, and do not treat it as the remote-playback fix.

## Debug — v0.8 remote playback

### Status

Incomplete investigation. WIP notes only. No fix, no regression test, no local code change. Working tree before this doc was clean (`git diff --stat` empty). Base is `beta` at `1b411215fca3210dd1dc3e08c8c72e20c2e4a978`. Comparison base for 0.7.8 behavior is tag `v0.7.8` (merge-base with `beta` is `5256fa49`, the 0.7.7→0.7.8 version bump).

### Symptom

v0.8 desktop remote mode cannot preview or play audio against the existing server. The v0.8.0 release was withdrawn. Latest published release is v0.7.8. Same machine, server on port 8790. Failure looks like playback never starts. Suspect the client stalls before any `/audio/{id}/stream` or `/preview` request, possibly behind an Upgrading UI.

### Scope for Codex

Desktop / local client remote playback only.

- Do not touch issue #97.
- Do not bump, redeploy, or restart production port 8790. Do not upgrade the server.
- Do not implement album artwork or covers. That is a separate track: capability `album_artwork`, `GET /api/v1/audio/{id}/artwork`. Paul has not approved it. Do not remove `remote_read_only()` early returns. Do not add `capabilities.artwork`.
- Do not bump `Cargo.toml`, pack a release, or delete GitHub releases or tags.
- Do not merge this handoff into `beta`.

### Hypothesis

Primary (check this first): the 0.8 client (`TARGET_SCHEMA` / schema v7) opening an 8790 schema-6 remote snapshot may mis-enter `upgrade_owned_snapshot` / album projection / `prepare_cached_state` and stall on a large library before any `/stream`. A full copy plus a per-row album backfill would look exactly like "can't preview".

Secondary, only if `/stream` or `/preview` is actually requested: `playable_path` → `RemoteClient::audio_source` → `StreamSource` (selection settle, stream vs preview, Range, `AudioCommand::Load`).

Code reading has **not** proved the primary hypothesis. A complete Plan B schema-6 file may skip the copy. That path was not executed. Codex must still prove or kill it on the real library before changing the gate.

### Evidence (code reading only)

Confirmed by reading current `beta` and `git diff v0.7.8..HEAD`. Nothing below was executed (`cargo test` was not run; no Windows repro; no HTTP trace against 8790).

**Zero content diff on the play HTTP path**

`git diff --stat v0.7.8..HEAD` does not list `app/audiolib-desktop/src/remote_mode.rs`, `crates/audiolib-remote/src/client.rs`, stream/route/snapshot-transfer sources, or the playback decoder's stream open. `remote_mode.rs` was included in a path-limited diff and produced no hunks. Server stream/preview sources are likewise unchanged in that range. `Cargo.lock` only moves workspace package versions 0.7.8 → 0.8.0.

`app/audiolib-desktop/src/controller.rs` is +221 lines (album UI / cover plumbing). `queue_artwork` and `publish_albums` return immediately when `remote_read_only()` is set. The album sidebar is hidden when `remote-mode` is set. Playback `read_info` is `read_info_impl(path, false, false)` in `crates/audiolib-audio/src/wav.rs`: it still stops at the `data` chunk and does not read ID3 artwork. `probe_with_artwork` is a separate API.

**Schema 6 no longer takes the "current format" branch**

- `crates/audiolib-db/src/schema.rs`: `SCHEMA_VERSION` is `7` (was `6`). `search_state_is_current` (around line 202): `user_version == SCHEMA_VERSION` calls `readable_format_ready`. Schema 6 falls through to the legacy check: `search_text_is_current`, `source_tags_ready`, `search_indexes == 1`, and four indexes (`idx_closure_descendant`, `idx_audio_files_not_present`, `idx_audio_search_facets`, `idx_audio_visible_tagged_folders`). The function body is otherwise the same as 0.7.8; only the constant changed, so schema 6 used to hit `readable_format_ready` and now hits the legacy check.
- `crates/audiolib-db/src/schema/current.rs`: `format_ready` requires `format_ready_at_version(..., SCHEMA_VERSION)` **and** an `album` column. `fast_upgrade_album` is the v6→v7 additive path (album column, batched `UPDATE audio_files SET album=audiolib_album_key(metadata_json)`, album index). `needs_upgrade` is true for any version other than 7.
- `crates/audiolib-core/src/compatibility.rs`: `TARGET_SCHEMA = 7`. Schema 6 stays a named read format: `PLAN_B_V6_FORMAT` (`ale-s6-cld1-t3-m2-p1`) and `PLAN_B_V6_INITIAL_FORMAT` (`ale-s6-cld1-t3-m2-p0`) are in `compatibility_manifest().snapshot_read_formats`.

**Where a false search-state result becomes a hang**

- `SqliteProvider::has_current_search_state` (`crates/audiolib-db/src/lib.rs`, ~729) opens the file read-only and calls `search_state_is_current`.
- `RemoteCache::prepare_cached_state_with_progress_cancellable` (`crates/audiolib-remote/src/sync.rs`, ~480): if that probe is true, it returns the cache unchanged. If false, it SQLite-backups the whole file into an owned staging copy, then `validate_snapshot_with_progress_cancellable`.
- `validate_snapshot_with_progress_cancellable` (~749): always runs `PRAGMA journal_mode=DELETE` on the staging file (small write, not the multi-GB copy). If `has_current_search_state` is true, it returns. If false, it reports `CachePreparation::Upgrading` and calls `SqliteProvider::upgrade_owned_snapshot_with_progress_cancellable`. Startup maps that to the Upgrading UI.
- `upgrade_owned_snapshot_with_progress_cancellable` (~758): returns early only when `version == SCHEMA_VERSION` (7) **and** `current_format_ready`. Schema 6 does not return early. It calls `migration::prepare_writable`, which tries `fast_upgrade_album` before a full storage migration. That UPDATE walks every `audio_files` row.
- `SqliteProvider::open_read_only` (~874): if `search_state_is_current` is false, copies into an `audiolib-readonly-*` temp directory and calls `upgrade_owned_snapshot`. If true, it keeps the original path.

Reading of a **complete** Plan B schema-6 library (tokenization 3, source-search and source-tags rev `"2"`, `search_indexes` 1, those four indexes — the same objects 0.7.8 `readable_format_ready` required) predicts `search_state_is_current == true`, so `prepare_cached_state`, `validate_snapshot`, and `open_read_only` would **not** copy or upgrade. That prediction is not a test result. If the real 8790 snapshot fails the legacy check, the copy/upgrade path above is the stall. Do not switch schema 6 over to `format_ready_at_version(..., 6)` without reading `ready_snapshot_schemas_3_through_6_read_without_retired_dto_fields` in `sync.rs`: that fixture is not a real Plan B schema 6, and tightening the gate would start copying it.

**Album projection (related, not shown to fail)**

`AUDIO_SELECT` ends with `COALESCE(af.album,'')` (`lib.rs`, ~2845). `audio_projection` and `album_expression` substitute `audiolib_album_key(af.metadata_json)` when `pragma_table_xinfo('audio_files')` has no `album` column. `register_casefold` in `schema.rs` registers `audiolib_album_key`. Readers go through `audio_projection`. This was not executed against a schema-6 snapshot.

**Handshake (related, not shown to reject 8790)**

`compatibility::library` (`crates/audiolib-remote/src/compatibility.rs`, ~67) accepts a descriptor when `schema_version` matches and `SnapshotFormat::is_readable_by` is true. A missing descriptor is allowed only for schema 3..=5. Schema ≥6 bytes without a declaration are rejected in `downloaded`. `RevisionCheck::classify` fails closed when `compatibility::library` fails. A 0.7.8 server that sends `snapshot_format` `ale-s6-cld1-t3-m2-p1` should still match the 0.8 read list. Not verified against the live `/library` body on 8790.

**Local diff**

No playback code was edited. `git diff --stat` before this document was empty. This commit adds only this handoff.

### Not verified

- No Windows repro. `<Windows acceptance host>` `local_release` 0.8 vs 0.7.8 was not run.
- No log of whether `/stream` or `/preview` is hit, and no Upgrading-UI observation.
- `has_current_search_state` / `open_read_only` / `prepare_cached_state` were not run on a real Plan B schema-6 file or on the 8790 snapshot.
- `cargo test`, Clippy, and `cargo fmt` were not run.
- Live server headers, token, and library size were not captured.

### Next steps (ordered)

1. On `<Windows acceptance host>`, repro with the installed `local_release` 0.8 client against the same 8790 server, then repeat with the 0.7.8 client. Do not upgrade 8790. Watch the UI for Upgrading / Tokenizing / Copying before any sound.
2. Log HTTP. If `/audio/{id}/stream` and `/preview` are **absent**, stop tracing the decoder.
3. Break or log `RemoteCache::prepare_cached_state_with_progress_cancellable`, `validate_snapshot_with_progress_cancellable`, `SqliteProvider::has_current_search_state`, `search_state_is_current`, `open_read_only`, and `upgrade_owned_snapshot_with_progress_cancellable`. Record schema `user_version`, whether the search-state probe is true, and whether a temp copy (`audiolib-readonly-*` or `.snapshot-job-`) appears.
4. If the probe is false, that is the stall: fix the smallest gate so a readable schema-6 Plan B snapshot is not copied or upgraded, and add a regression that a real Plan B schema-6 file stays `user_version` 6, keeps no `album` column, and `open_read_only().database_path()` is the cache file.
5. If the probe is true and sync finishes but there is still no audio, then trace `playable_path` (`app/audiolib-desktop/src/remote_mode.rs`) → `RemoteClient::audio_source` → `StreamSource` (settle, `/stream` vs `/preview`, Range, auth, `AudioCommand::Load`). Those functions match 0.7.8 today; a bug there would be new evidence, not a diff in those files.
6. If `/stream` is hit and returns an error, trace URL and auth only. Still no artwork work.

### Out of scope

Cover / artwork API, `capabilities.artwork`, remote cover fetch, issue #97, port 8790, version bump, release packaging, merge to `beta`.
