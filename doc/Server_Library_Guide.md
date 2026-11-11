# Managing sound libraries on the server

How `audiolib-server` adds library roots, scans audio, imports sidecar spreadsheets, serves album covers, and publishes a snapshot to clients. Install and process setup are in [Server deployment](Server_Deployment.md).

The server exposes one SQLite library plus the audio files under its configured roots. It reads those files. Scans, Metadata edits and root removal change the database. Source audio files stay where they are.

v0.9.3 does not change this contract and does not add a schema migration. Ordinary local writers use schema 8. A server that has completed the explicit schema 9 migration writes `ale-s9-cld1-t3-m2-p1-a1-sd1`. Clients open a validated read-only snapshot of schemas 3 through 9.

## Where roots are stored

Each root has a unique name (compared case-insensitively) and an absolute `server_path` on the server. Optional `webdav_url` is reported to clients.

- A fresh install reads `[[roots]]` from the TOML config.
- The LAN admin page (`/admin`, section 库根目录) and the client root API write `admin-settings.json` beside `library_db`. Once that file contains a root list, it replaces the TOML list until the next saved edit.
- Changes apply without restarting the process.

The admin page answers only a LAN-direct request: a private peer, no forwarding headers, and a LAN `Host`. It has no password. `manage_mode` does not hide the page; `trust_lan_peers = false` disables it. The Docker image ships with that setting and with `manage_mode = "off"`.

## Add, move and remove a root

A new root must already exist, must be absolute, and must neither contain nor sit inside another root. Overlap is rejected. Adding or removing a root while a scan is running returns 409 `BUSY`.

Three ways to add one:

1. Edit `[[roots]]` before `admin-settings.json` has taken over.
2. On the admin page, browse the server disks (read-only) and add a directory. Mount network shares with the operating system first; the server does not mount them.
3. In the desktop client, open **Manage sound library** (Ctrl+D), select the active server, and use **+ Add sound root** / **＋ 添加音效库**. **Browse…** calls the server directory listing. **Save sound root** sends the name and absolute path.

The client shows those buttons when this server is the active source, the snapshot is open, and `/api/v1/capabilities` says this client may manage the library. Otherwise the page is view-only. Default `manage_mode = "lan"` allows a LAN-direct client and returns 403 `MANAGE_FORBIDDEN` for an external token. `manage_mode = "off"` refuses every client, which is the bundled Docker config. `manage_mode = "all"` also allows an external token; turn that on only for clients trusted to browse server disks and scan them.

**Edit resource path…** / **修改资源路径…** moves an existing root to another absolute directory. The server relocates the indexed tree in one transaction. A missing or invalid target rejects the whole change. File ids, Metadata and annotations stay. A WebDAV URL stored for the old directory is cleared. Drive roots and filesystem roots cannot be relocated. The name stays the same. This is also unavailable during a scan.

**Delete sound root…** / **删除音效库…** removes that root's folder and audio records from the library, including annotations, and drops the root from the saved list. The directory and the audio files on disk are left in place. Before the delete, the server writes `backups/before-root-remove-<uuid>.db` beside the library. A rescan can index the same files again; they receive new ids, and the removed annotations do not come back.

`POST /api/v1/audio/remove` and `DELETE /api/v1/folders/{id}` are the same kind of database-only delete for individual records or one folder tree. Both require `manage_mode` and are refused while a scan runs.

The main window's **Incremental folder scan** and **Full folder rescan** commands are for a local library. In server mode they are disabled. Server scans start from **Server scan** / **服务器扫描** on the same management page, or from the admin page.

## Full and incremental scans

`POST /api/v1/scans` accepts:

| Field | Meaning |
| --- | --- |
| `root` | Configured root name. |
| `subpath` | Optional directory inside that root, using `/`. Empty means the whole root. |
| `mode` | `incremental` (default) or `full`. |
| `workers` | Metadata threads, 1 through 16. Omit, `null` or `0` to let the server choose. |

The server always walks subdirectories. At most one scan runs. `GET /api/v1/scans/current` returns the active scan or `null`. `POST /api/v1/scans/{id}/pause`, `resume` and `cancel` control it. The client labels are **Incremental (new and modified)** / **增量（新增和修改）** and **Full (re-read, detect missing)** / **完整（重新读取并检查缺失）**.

Scanned extensions are `.wav`, `.flac`, `.ogg` and `.oga`. Embedded WAV Metadata includes BWF `bext`, iXML, INFO, ID3, XMP and read-only Soundminer SMED. FLAC and Ogg Vorbis/FLAC comments are extracted, including repeated values and Chinese text. `.opus` is not part of this walk. Opus playback is unsupported even where a tag probe can see an Opus header inside an Ogg file.

Incremental mode probes a file again when its size, modification time or Metadata extractor version differs from the stored row. It does not mark absent files as missing. Full mode reads every file under the selected directory. After a completed full scan with zero per-file errors, files that were not seen are marked missing, unless the directory listing is empty (hidden and housekeeping entries aside). An empty listing is treated as an unmounted volume, and existing rows are left alone. Cancel, a read error, a symlink, or any recorded per-file error also skips that missing-file pass. A symlink is reported as `Symbolic link was not traversed; missing-file reconciliation is disabled` and is not followed. An unreachable root fails the scan and does not change records.

While the scan runs, the library revision clients poll is held. It moves once at the end if the scan changed the library. The first 100 issue strings are kept on the session (`error_details`); further issues still increment the error count (`errors_truncated`). `GET /api/v1/scans/{id}` and scan history return those issues. The client server page lists the latest 20 sessions.

User annotations already on a record are kept across a rescan. A spreadsheet imported by the scan is a separate Metadata source, described next.

## Sidecar spreadsheets (sidecar-v1)

After the audio workers finish, the same scan imports spreadsheets it found under the scanned directory. This path is automatic. It is separate from **Import vendor metadata…**, which previews CSV, TSV, TXT, XLS or XLSX and waits for a confirmed commit.

Automatic discovery accepts `.csv`, `.tsv`, `.xls` and `.xlsx`. It skips `._*`, `~$*`, `@syno*` and `.DS_Store`. Each table must be from 1 byte through 50 MiB. The stored source kind is `sidecar-v1`. The scan records a per-file issue and keeps the last good Metadata from that file when:

- the table is empty, larger than 50 MiB, or changes while it is being copied
- a row is truncated
- no sheet has both an identity column (filename or relative path) and at least one Metadata column

Messages look like `Validation: Automatic table must contain 1 byte through 50 MiB`, `Validation: Automatic table contains truncated rows; last good Metadata retained`, and `Validation: No recognized identity and Metadata columns; last good Metadata retained`.

### Which WAVs a table may describe

Candidates are WAV files (`.wav` only) that this scan successfully saw and that are not marked missing. Matching uses the file name, case-insensitively. A row's identity cell must be a single file name. A value that contains `/` or `\`, a drive letter, or `.` / `..` is skipped. When both a filename column and a relative-path column are mapped and they differ after case-folding, the row is unmatched. The scan does not search the rest of the library for that name.

Scope:

1. The table's own directory and every subdirectory under it.
2. Sibling directories, only when the table's parent folder is itself named `{stem}--Docs` or `{stem}--Sounds*` (the comparison is case-insensitive). `*` means the folder name continues after `--Sounds`, as in `Pack--Sounds 48k`. The stem before `--Docs` or `--Sounds` must be non-empty.

Sibling folders are other children of the same parent, inside the scan root, with the same stem and a `--Docs` or `--Sounds` marker. Each sibling's whole subtree is included. A table that sits in the scan root itself does not gain siblings; its scope is that root's subtree. A table nested inside `Pack--Docs/notes/` does not gain siblings either, because `notes` is not a Docs/Sounds directory. Two packs beside each other do not cross-match.

```text
/path/to/audio/Pack/Pack--Docs/metadata.xlsx
/path/to/audio/Pack/Pack--Docs/nested/extra.wav
/path/to/audio/Pack/Pack--Sounds/door.wav
/path/to/audio/Pack/Pack--Sounds 48k/room.wav
/path/to/audio/Other/Other--Sounds/door.wav
```

With the scan root at `/path/to/audio` or `/path/to/audio/Pack`, `metadata.xlsx` may describe `extra.wav`, `door.wav` under `Pack--Sounds`, and `room.wav`. It does not describe `Other--Sounds/door.wav`. If the scan root is only `Pack--Docs`, sibling Sounds folders are outside the scan and are not candidates.

A name that occurs once in that scope is applied. A name that occurs twice is ambiguous and is not applied. A name that occurs zero times is unmatched. When any row is unmatched or ambiguous, every previous contribution from that spreadsheet is kept and the new unique matches are added beside it. When every row associates cleanly, that spreadsheet's own older contributions are replaced by the new ones. Annotations typed by a user stay in their own store either way.

Correct the spreadsheet and scan again, or roll the source back from the explicit vendor-import history, to drop a bad automatic value. A damaged or missing spreadsheet does not delete the last good values by itself.

## Album covers

Embedded JPEG and PNG pictures are read from the audio file when a client asks for them. They are not written into SQLite, FTS5 or the snapshot file. The server cache lives in an `artwork` directory beside the library. A truncated cache file is discarded and built again.

- `GET /api/v1/audio/{id}/artwork` returns one file's embedded picture.
- `GET /api/v1/albums/{id}/artwork` returns the shared album picture. Directory-scoped clients send `folder_id` or `unassigned_directory`, plus `side` when they want a bounded size. The request is tied to the library id and the album namespace.

In **Manage sound library**, covers are under **Album cover in this folder**. In server mode that block is on **Cache and export**. **Show covers**, **Set folder image**, **Remove folder cover**, **Restore folder default** and **Reload folder cover** are client-side choices. Custom images live on that PC under its artwork-overrides directory. They are not part of the snapshot and do not sync to another computer. From v0.9.2, a client that restarts onto a cached snapshot keeps showing the cover it already has on disk while `/api/v1/capabilities` is still loading.

A full decode queue returns 503 `BUSY` with `Retry-After: 1`. The client waits and keeps the current image.

## What a client sees after sync

Server mode downloads `GET /api/v1/library/snapshot` into a read-only snapshot under the client's remote cache. Search, folders and filters run on that file. The server holds the revision during a scan, then publishes one new revision if anything changed, so the client does not resync on every batch. **Sync now**, startup, switching to the server, and Refresh (F5) compare library id and revision. The same revision only refreshes the check time. `AudioLibCli remote sync --force` downloads again.

The snapshot contains the indexed rows: paths, embedded Metadata, sidecar-v1 fields that were applied, user annotations, folder membership and missing flags. It does not contain cover bytes, playback history or the client's local working library. Playback history stays on each computer. Metadata and management writes that the server accepts go through HTTP and show up in a later snapshot.

A file that failed to scan, or that the scan has not seen yet, is absent until a later scan indexes it. A clean full scan can mark a deleted file missing; the row remains, with `missing` set, until something removes the record. Removing a root or a record deletes those rows. After the next sync the client stops listing them. The WAV/FLAC/Ogg file is still on the server disk.

A schema 9 library can still serve older clients a separate WAV-only schema 8 snapshot while `legacy_snapshot_enabled` remains on (the default). Those clients do not see non-WAV rows in that projection, and they cannot write the schema 9 source. Turning the compatibility snapshot off leaves them on their last valid cache. Current clients that accept schema 9 receive the native snapshot.

Preparing a large snapshot can take longer than `snapshot_wait_secs` (default 90). The request then returns 503 `BUSY` with `Retry-After` while preparation continues. The client retries.

## Scan errors

A bad file is one issue on the scan session. The rest of the walk continues. Because the error count is no longer zero, a full scan does not mark other files missing. The first 100 issue texts are stored; the numeric error count includes the rest.

| Situation | Recorded message |
| --- | --- |
| A WAV chunk claims more bytes than the file contains | `Audio: Truncated WAV chunk …` followed by the chunk id |
| An `.ogg` or `.oga` identification packet is not Vorbis, Opus or FLAC | `Audio: Unsupported Ogg audio identification packet` |
| An Ogg page header is not a version-0 `OggS` page | `Audio: Invalid Ogg page header` |
| The container is neither WAV, FLAC nor Ogg | `Audio: Unsupported audio container; expected WAV, FLAC or Ogg` |

Short or damaged Ogg pages can also be warnings inside a file that still indexes. Those warnings are not the hard errors above. Fix or remove the offending file and scan that folder again. An incremental scan will re-read a file whose size or modification time changed.

## Troubleshooting

- **`/api/v1/health` is `starting` or `error`.** Wait for `ok`. `error` with `The library could not be opened; the server keeps retrying` usually means `library_db` is missing, unreadable, or the operating system has not granted the process access to the volume. Details stay in the server log.
- **503 `UNAVAILABLE`.** The library is not open yet. This is separate from 503 `ROOT_OFFLINE`, which means one configured root did not answer the background stat in time. Check the root's `status` on `GET /api/v1/roots` and that the share is mounted.
- **403 `MANAGE_FORBIDDEN` or the client root buttons stay disabled.** `manage_mode` is `lan` or `off`, the caller is not LAN-direct, or `/capabilities` has not loaded. The Docker example config sets `manage_mode = "off"` until you replace it.
- **409 `BUSY` while editing roots.** A scan is running or paused. Cancel it and wait for it to finish, or wait for normal completion. Pausing does not release the scan lease.
- **503 `BUSY`.** Stream, preview or cover slots are full, or a snapshot is still being prepared. Honor `Retry-After`.
- **503 `TRANSCODER_UNAVAILABLE`.** Only `preview?codec=flac` when `ffmpeg_path` is missing. Original streaming still works.
- **A spreadsheet did not fill in descriptions.** Confirm the file sits directly in a `{stem}--Docs` or `{stem}--Sounds*` directory if it should see a sibling tree, that the scan root includes that parent, that identity cells are bare WAV file names, and that each name is unique in scope. Ambiguous and unmatched rows keep the previous sidecar values.
- **Covers are blank after a server-mode restart.** v0.9.2 and later reuse the on-disk thumbnail while capabilities load. An older client needs one successful sync before it requests covers. Custom images exist only on the PC that set them.
- **Clients still show the old list.** The scan's revision is published when the scan ends. Sync again. A snapshot of a schema 9 library served to an older client is WAV-only while the compatibility snapshot is enabled.
