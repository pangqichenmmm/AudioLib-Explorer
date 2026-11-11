# Handoff: server-mode album artwork (phase 1)

> Historical phase-1 handoff. The completed server/client implementation and
> subsequent validation are documented in [Album artwork](Rust_Artwork.md) and
> [Rust validation](Rust_Validation.md). Pending checks below describe the handoff state.

Capability key is exactly **`album_artwork`** (revision ≥ 1). Do not rename it to `artwork`. The client will use the same name.

## Goal / agreed API

- Advertise optional compatibility feature **`album_artwork`** at revision **1** on `GET /api/v1/capabilities` (`compatibility.features`). A server that omits it must cause clients to skip the request. A missing feature never refuses sync.
- Endpoint: **`GET /api/v1/audio/{id}/artwork`** (HEAD included). Optional query **`side`** is a thumbnail long-side bound, decimal `1..=512`. Omitted `side` returns the embedded JPEG or PNG.
- Resolve the audio id from the library, read that file from disk, extract the embedded cover with `audiolib_audio::probe_with_artwork`. Bounds match the desktop decoder: encoded ≤ 8 MiB, decode width/height ≤ 4096, decode allocation ≤ 64 MiB. Only `image/jpeg` and `image/png` are served.
- On-demand extract. **Do not** store image bytes in the main SQLite library or in snapshots.
- Optional disk cache under the server data directory: `<library directory>/artwork`, keyed by library id, audio id, canonical path, source file ETag, side, and algorithm version. About 256 MiB, oldest files dropped.
- Success: `image/jpeg` or `image/png`, strong `ETag`, `Cache-Control: private, no-cache`. `If-None-Match` → 304.
- No cover, missing file, or extract/decode failure: **404** `NOT_FOUND` (not 5xx). Invalid `side`: 400 `VALIDATION`. Root offline stays 503 `ROOT_OFFLINE`, same as `/stream`.
- Schema 6 is fine: lookup is by file id and path. It does not use the schema 7 album column.

## Done

- `album_artwork` revision 1 is in `audiolib_core::compatibility::FEATURES` and therefore on `/capabilities`.
- Route is wired in `server/audiolib-server` (not under the 30s timeout, same as stream/waveform).
- Extract, thumbnail, ETag, disk cache, 404 mapping, and “bytes stay out of SQLite” are implemented.
- Unit tests in `artwork.rs` passed on Linux (`x86_64-unknown-linux-gnu`): side parsing, JPEG/PNG pass-through, thumbnail bounds, oversized rejection, cache prune, feature name is not `artwork`.
- HTTP test `server/audiolib-server/tests/artwork.rs` covers capability presence, happy path, 304, HEAD, `side=8`, plain file 404, unknown id 404, bad `side` 400, corrupt file 404, missing file 404, and SQLite does not contain the PNG. It was compile-fixed in this handoff; re-run it before treating the PR as green if CI has not.
- API notes: `doc/api/README.md` §8.2, plus short mentions in `server/README.md` and `server/README.zh-CN.md`.

## TODO for Codex

- Re-run the HTTP test below and strict Clippy for `audiolib-server` / `audiolib-core` if this handoff’s last test run did not finish green.
- Linux `cargo test` needs `--target x86_64-unknown-linux-gnu` because `.cargo/config.toml` defaults to `x86_64-pc-windows-msvc`.
- No Mac-only asset is required; tests build a tiny WAV with an ID3 PNG.
- Do not start phase 2.

## Key files

- `crates/audiolib-core/src/compatibility.rs` — feature constant and `FEATURES` entry
- `server/audiolib-server/src/artwork.rs` — extract, bounds, cache, ETag
- `server/audiolib-server/src/routes.rs` — `GET /audio/{id}/artwork`
- `server/audiolib-server/src/lib.rs` — module
- `server/audiolib-server/src/state.rs` — `OpenProgress::clear` is `pub(crate)` so extract time is not logged as a hung open
- `server/audiolib-server/Cargo.toml` and `Cargo.lock` — `image` 0.25.10 (png, jpeg only)
- `server/audiolib-server/tests/artwork.rs`
- `doc/api/README.md` §8.2

## How to test

```sh
cargo test -p audiolib-core --lib compatibility --locked --target x86_64-unknown-linux-gnu -- album_artwork
cargo test -p audiolib-server --lib artwork --locked --target x86_64-unknown-linux-gnu
cargo test -p audiolib-server --test artwork --locked --target x86_64-unknown-linux-gnu
```

On Windows, drop the `--target` flag (the repo default target is already MSVC).

## Out of scope

- Phase 2: album-name sidebar cover endpoints, scan-time bulk pregeneration (leave default off), desktop client artwork requests.
- Issue **#97**. Do not comment on it or edit that design thread.
- Production deploy configs and the port **8790** upgrade. Do not change deploy configs.
- Client desktop play/stream fixes.
- The client and server must keep the capability name **`album_artwork`**. Do not rename it.
