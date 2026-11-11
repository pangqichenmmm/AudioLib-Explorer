# Shared albums and Metadata storage (schema 8)

The current writable format is `ale-s8-cld1-t3-m2-p1-a1`. It adds the
`album_entities: 1` format feature and retains Metadata as original JSON TEXT.
Metadata compression is deferred; outer snapshot zstd transport remains enabled.
Upgrade clients and servers together: older binaries that support schemas only
through 7 cannot open
a native schema-8 snapshot. HTTP API v1 remains additive.

## Album ownership

An album belongs to a Metadata library or a folder, rather than a selected WAV.
The trimmed, case-sensitive WAV Metadata `library` value identifies a shared
Metadata album. With no such value, the registered scan root's first child folder
identifies the folder album; files directly under a root use that root. Records
with no registered folder share an unassigned folder entity. Metadata and folder
albums are different kinds even when their display names match. Directories can
contain multiple Metadata libraries, so directory names never replace a declared
Metadata library.

`audio_files.album_id` references `albums`; the old duplicated `album` text column
is retired. Owned DTOs retain `AudioFile.album` as the declared Metadata value and
add `album_id` / `album_entity`. Search accepts either the legacy album name or a
positive album ID, never both. Sidebar facets carry their ID and matching count
in one provider query and retain the current search and other filters.

The two sidebar indexes are cross-filters: folder counts retain the keyword and
album conditions while ignoring the selected folder itself; album counts retain
the keyword and folder conditions while ignoring the selected album itself.
"All sounds" clears both folder and album scope, including a retained task album
binding, and keeps the keyword and other filters. Audio-favorites navigation uses
the same scope reset while retaining its favorites-only condition.

Album IDs are scoped by a persistent random namespace. An allocator high-water
mark prevents ID reuse after removal and restore. Root relinking preserves the
folder entity; a file moved into another album adopts that album. Reparenting
registered roots recomputes affected fallback memberships. Backup/restore retains
identity; import as a new library creates a fresh namespace. Restoring updates
the provider's cached identity before readers and UI generations are renewed.

Personal search tasks bind album filters to that namespace and semantic owner.
Native-server upgrades resolve all open and saved filters before executing a
numeric ID. Missing owners remain visibly unavailable; explicit reselection can
choose a new album even when it reuses an old numeric ID in another namespace.
Workspace format 2 prevents older clients from ignoring these conditions; see
[search-task recovery](Rust_Search_Tasks.md).

Pictures remain outside SQLite, FTS5 and snapshots. Automatic covers are scoped
to a shared album within the selected WAV's immediate source directory. The
directory filter applies before selecting up to 64 sorted, present members; a
member without a picture can reuse another picture in that same album and
directory. Different subdirectories never borrow one another's detail cover,
even when their Metadata declares the same Library. Album entities and counts
remain shared; this cover context adds no schema migration or Metadata rewrite.

Manual selection, hiding and restore-to-embedded use the same source, album and
immediate-directory context in local and server modes. Older whole-album choices
remain read-only inherited defaults until overridden by a directory choice.
Restoring the directory default records an ignore-inherited receipt and uses
its embedded artwork without deleting the older preference for other folders.
Native server requests use the optional `album_folder_artwork: 1` capability and
validate library, namespace and directory identity; the database stays at
schema 8 with original Metadata TEXT. Valid BOOM SMED Picture tags and ID3 APIC
can supply embedded images, including distinct Construction Kit and Designed
posters within one Library. See [artwork](Rust_Artwork.md) for cache and preference
details.

## Original Metadata TEXT

New libraries, scans, migration outputs and new server snapshots store
`metadata_json` as UTF-8 TEXT. The application preserves the original JSON bytes,
including unknown fields, whitespace and key order; it does not parse and
reserialize JSON merely to change album ownership. Images stay in their separate
cover store. DTOs, API replies and JSONL/CSV export retain their ordinary Metadata
representation.

The experimental `ale-s8-cld1-t3-m2-p1-a1-z1` format remains a strictly recognized
read-only source, with `metadata_storage: 1` in that source descriptor. Its bounded
decoder checks the `ALEMD` header, declared length, stream end, checksum, trailing
bytes and UTF-8. Malformed or unknown encodings are errors, never empty Metadata.
No production writer creates new compressed values. Opening such a library for
local/server writes requires an owned conversion to the plain target. Existing
validated remote caches can remain read-only in their actual old format.

SQL code maintained by this project retains
`audiolib_metadata_json(metadata_json)` for legacy source projection. External
tools reading an experimental `z1` source should use the application API/export;
new plain-target libraries use ordinary SQLite TEXT. Unknown tables, implicit
rowids, triggers, views, generated columns, CHECK constraints and
uniqueness/collation rules remain intact. A constraint that cannot be retained
causes conversion to fail without replacing the source.

FTS5 continues to index searchable fields independently of cover images. The
performance target remains **P95 latency <500ms for 1M+ records**. The new TEXT
candidate's final space, transport and performance acceptance is pending; earlier
compression measurements do not establish its results.

### Deferred compression experiment

Before compression was deferred, a 909,342-record `a1-z1` rehearsal reduced the
SQLite file from 10,611,302,400 to 5,400,199,168 bytes (49.11%) and retained every
original Metadata byte. The corresponding zstd level 3 / eight-worker snapshot
grew from 950,903,523 to 2,033,256,351 bytes. Those storage and transport results
belong to the deferred experiment, not the current TEXT candidate. Backup and
staging files were additional storage outside the final-file comparison.

The same experiment's Ryzen 7 9800X3D / Windows local SSD / warm-cache measurements
showed result-page P95 at 99–101 ms before and 109–113 ms after compression; a
separate 1M-record synthetic fixture measured 117.7 ms (212.2 ms including directory
facets). These measurements exclude provider opening and complete UI latency and
must not be reported as acceptance of the new TEXT candidate.

## Migration and recovery

Recognized schemas 3–7 are read-only sources until migration. Migration reviews
the source format and extension dependencies, creates an online rollback backup,
copies into an owned candidate, backfills albums, retains Metadata as TEXT and
rebuilds FTS5. Business digests compare decoded original Metadata bytes; new album
bindings are then included in the target digest. Full format and data validation
precede publication. An unsupported dependency or malformed source fails without
replacing the original library.

For an experimental `a1-z1` writable source, the owned copy decodes each value to
TEXT without changing JSON bytes, album IDs, namespace, high-water marks or
legacy scope. Publication requires a complete TEXT target with the old Metadata
storage feature removed. An unfinished older-recipe job must first finish or
roll back with its matching binary; a new recipe never takes over that job by
editing its markers.

Album backfill creates a uniquely named covering index for folder ancestry in
the owned transaction before processing members, then drops only that index.
This avoids a full closure scan per WAV before permanent indexes are installed.
Cancellation rolls the transaction back; unknown schema objects stay intact.

Copying into the new candidate physically repacks storage. The final phase merges
FTS segments, but does not run an unrestricted VACUUM that could renumber implicit
rowids in unknown extension tables. Backup and staging files require additional
free space and are retained for recovery. Removing them is a separate recovery
decision, never an installation cleanup step.

Remote migration changes only an owned local copy and retains its source-format
descriptor. Old-source projections retain a local namespace and member IDs across
revisions. Native schema-8 snapshots use the server's namespace. Namespace and
library headers prevent the same numeric album ID from selecting a cover from a
different library or imported database.
