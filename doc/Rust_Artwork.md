# Album artwork

Artwork is scoped to a shared album within the selected WAV's immediate source
directory. Selecting another member in that same directory retains its cover;
another subdirectory has independent automatic and manual choices, even when
both declare the same Library. A WAV supplies pixels, not a per-file preference.
Shared Album entities, schema 8 and original Metadata TEXT remain unchanged;
Metadata compression is deferred. See
[shared albums and Metadata storage](Rust_Shared_Albums.md) for the schema-8
ownership, namespace and migration contracts.

Encoded images and decoded pixels never enter SQLite, FTS5, JSONL/CSV exports or
server snapshots. Reading or managing a cover does not change source WAV files,
Metadata or annotations.

## Display and management

Select a sound, then open **Sound library management → Album cover in this folder ▸**. In
server mode these controls appear under **Cache and export** for the connection.
Both modes offer:

- **Show covers**, persisted as `show_album_artwork` (old settings default on).
- **Set folder image**: PNG/JPEG up to 8 MiB, 4096 pixels per dimension and
  64 MiB decode budget. The client stores a thumbnail up to 512 pixels; moving the
  original image does not break the choice.
- **Remove folder cover**, hiding custom and embedded artwork for this album in
  the current immediate directory.
- **Restore folder default**, ignoring inherited older album choices and using
  this directory's embedded artwork without changing another directory.
- **Reload folder cover**, revalidating only the current directory's cover.
- **Clear cover thumbnails**, clearing generated client images while retaining
  custom images, hidden markers and migration receipts.

The current target shows the Album name and immediate directory. Custom choices
live under `<user data>/artwork-overrides/`; keys bind the local library namespace
or remote library identity, semantic album owner and normalized source directory.
They do not follow an individual WAV or a reused numeric folder ID. LAN/external
profiles of the same server library share choices. Stable semantic identities
retain choices across older-source projections and the native schema-8 upgrade.
Choices belong to this PC and are outside `.db` backup/restore and inter-client
synchronization.

Historical album-name and per-WAV choices migrate by copying into the legacy
shared identity, with their original files retained.
Identical choices merge; conflicting images or image/hidden choices retain every
original and display a conflict notice so the user can choose. No arbitrary WAV
wins a conflict. Explicit restore-to-embedded stores a migration receipt, so
retained old files cannot resurrect a removed choice after restart. These older
whole-album preferences are now read-only inherited defaults. An explicit
directory image, hidden choice or valid restore receipt takes precedence;
restoring one directory does not delete the older preference for other folders.
Damaged explicit preferences are reported instead of silently enabling inheritance.

## Shared extraction and caching

Default extraction filters the shared album to the selected WAV's immediate
directory before selecting up to 64 present members in ascending audio ID order.
The first valid embedded picture supplies that directory, independent of the
selected member, search query and sidebar page. A later directory is not lost
because its members fall beyond the whole album's first 64 IDs. Valid ID3 APIC
and BOOM SMED Picture tags can supply pixels, including different Construction
Kit and Designed posters within one Library. A new scan or explicit reload
revalidates changed pictures. Explicit directory choices precede inherited ones.

Generated thumbnails live under `<cache>/artwork/` with a 256 MiB budget. Keys
identify the album/directory context, namespace and indexed folder identity when
available. Local validation includes member IDs, source paths, sizes and recorded
modification times; HTTP uses context-bound ETags. The sidebar retains at most
128 source/directory hints, showing the last successfully browsed directory for
an album. It reads cached pixels for the first 12 album rows. Hint validation or
an owned representative `get_audio` lookup runs on the search worker, without
opening WAVs or issuing per-album search/count queries. Sidebar hints never
supply detail pixels to another directory.

A dedicated coalescing worker performs extraction and HTTP outside the Slint event
loop and audio callback. Only the same album and directory reuse in-flight work.
Folder ID, normalized parent, namespace, source generation, epoch, cancellation
and shutdown checks reject stale cache writes and event-loop publication. Extraction and
HTTP run outside the cache mutex. Admitted manual edits retain their original
library/album/directory destination across source changes; close drains those edits off the
event loop. Picker cancellation and rejected images resume automatic loading.

## Server compatibility

Native schema-8 directory requests require both optional
`compatibility.features.album_entity_artwork == 1` and `album_folder_artwork == 1`.
They use `GET /api/v1/albums/{id}/artwork?folder_id={folder-id}&side=512`. Requests bind both
`X-AudioLib-Library-Id` and `X-AudioLib-Album-Namespace`; the client validates
`X-Library-Id`, `X-Album-Id`, `X-Album-Namespace`, `X-Album-Folder-Id` and
`X-Album-Directory-Identity` on both 200 and 304 replies. The directory identity is
the normalized source parent's lowercase UTF-8 hex; it detects path changes even
when numeric IDs remain unchanged. Missing, duplicate or mismatched identity
headers cannot authorize an image or cached 304 result.

Historical members with `folder_id=NULL` use `unassigned_directory` instead of
`folder_id`, restricted to that same album and normalized immediate parent. An
empty parent is a valid flat-relative context, not a whole-album request. This
mode echoes the directory identity without a numeric folder header. Native
servers without the directory capability must be upgraded; the client does not
downgrade to a whole-album response or an unbound per-audio request.

For source schemas 3–7, the client validates and upgrades only an owned copy. It
filters candidates to the same immediate directory before using the older
`/audio/{id}/artwork` capability
instead of sending local projected album IDs. Missing capability, 404 or 405
produces an empty embedded display while custom images remain usable. Artwork
never downloads or seeks through a remote audio stream to find a picture.

Server extraction uses the already-open file handle that passed root/path checks.
Two separate artwork slots bound decoding without occupying playback slots.
A full queue returns `503 BUSY` with `Retry-After: 1`. See
[API §8.2](api/README.md).

## Acceptance boundaries

The following is historical acceptance of the earlier whole-album implementation,
not acceptance of the current directory candidate. The authorized production
deployment on 2026-10-27 updated the Mac server to the
then-current 0.8.0 development build. The existing launcher, its signature, launchd
configuration and file-access chain were retained. Schema 6 was upgraded in place
to schema 7 after validating an independent rollback backup and an upgrade copy.
The library identity and complete business-data digest remained unchanged.

Real Windows acceptance displayed an embedded cover from that server and played
its PCM preview through WASAPI, including pause, seek and Stop/Play. Older servers
without `album_artwork` still use the fallback described above. No GitHub release
was published by this deployment.

See [Validation](Rust_Validation.md) for verified coverage and remaining limits.
