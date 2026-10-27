# Rust Remote (Server) Mode — Client

AudioLib Explorer can browse a library hosted by `server/audiolib-server` instead of the
local working library. The client side is the `crates/audiolib-remote` crate (blocking
HTTP client, snapshot sync, WebDAV mapping, audio cache, secrets) plus the desktop
"Data source" dialog and the `AudioLibCli remote` commands. The server contract itself is
documented in [doc/api/README.md](api/README.md); this document describes client behaviour.

In server mode the client downloads a validated SQLite snapshot and opens it with the
ordinary read-only `SqliteProvider`, so search, folders and filters keep local-mode
performance. Annotation edits and play counts are written to the server and applied to the
snapshot by revision (see *Writes in server mode*); permitted scans and removals use
the server management API. The local working library and its settings are never opened,
modified or replaced while in server mode; switching back to local mode reopens it as before.

## Settings and secrets

`settings.json` gains a `data_source` section (all fields have serde defaults, so older
settings files load as local mode):

```json
"data_source": {
  "mode": "server",
  "server_url": "http://192.0.2.10:8790",
  "webdav": {
    "base_url": "https://nas.example/dav",
    "username": "sound",
    "root_urls": { "SFX": "https://nas.example/dav/SFX" }
  },
  "poll_interval_secs": 60,
  "auto_sync": false
}
```

- `server_url` is the normalised server base URL `scheme://host[:port][/prefix]`; the
  client appends `/api/v1` once (an entered URL that already ends in `/api/v1` is accepted
  and the suffix is not doubled). See *Server address and HTTPS* below. Existing values
  such as `http://192.0.2.10:8790` stay valid unchanged (same API base and cache key),
  so there is no migration.
- `poll_interval_secs` (default 60, `0` = off, otherwise 10–86400) is the background
  revision check interval; `auto_sync` downloads a new snapshot automatically instead of
  only showing a notice. Both are edited on the connection page of the 「音效库管理」
  window and apply to every server.
- The access token and the WebDAV password are **not** stored in `settings.json`. They
  live in `<data dir>/remote/secrets.bin`, protected with Windows DPAPI for the current
  user (`ALXS-DPAPI-1` header; a plain fallback with owner-only permissions is used only
  on non-Windows hosts). Saving empty secrets deletes the file. Secrets are never logged,
  never included in exports, and `Debug` output of every client type redacts them.
- The access token is **optional**. With an empty token the client sends no
  `Authorization` header on any request (health, library, roots, snapshot, audio stream,
  ETag revalidation), and the CLI (`remote test`/`sync`) and the desktop (dialog, sync,
  polling, playback) work against a server that does not require one. The connection
  page shows an info line above the connection report ("No access token: requests carry
  no Authorization header"), and `remote test` lists the same note in `warnings`. If such a
  server answers `401`, the error is `RemoteError::TokenRequired` and every surface says
  "The server requires an access token (401)" instead of "token rejected". A token that is
  entered is stored with DPAPI as before and sent as `Authorization: Bearer <token>` on
  every request (API, snapshot, `/stream`, `/preview`, `/waveform`, ETag revalidation).
  The server ignores it on direct LAN connections (API §2.2), so one saved profile with a
  token works with both the LAN and the external address.
- `<data dir>` is `AUDIOLIB_DATA_DIR` when set, otherwise `%LOCALAPPDATA%/AudioLibExplorer/v1`.

### Server profiles (design `server-mgmt-gui.md` A0)

`data_source` also holds several saved servers and the active source:

```json
"data_source": {
  "mode": "server",
  "server_url": "http://192.0.2.10:8790",
  "active": "default",
  "profiles": [
    { "id": "default", "name": "192.0.2.10:8790", "server_url": "http://192.0.2.10:8790",
      "kind": "lan", "group": "default", "transcode": false },
    { "id": "p-3f9c0a1b2d", "name": "192.0.2.10:8790 (external)",
      "server_url": "https://audiolib.example.com:8443", "kind": "external",
      "group": "default", "library_id": "…", "transcode": true,
      "last_test": { "at": 1704067200, "ok": true, "summary": "…" } }
  ]
}
```

- `active` is `"local"` or a profile `id`. `mode`/`server_url` (and the top-level token)
  always mirror the active profile, or the last used one in local mode, so older clients
  keep working. A profile may carry its own `webdav` block (same shape as
  `data_source.webdav`); without it the global one applies.
- Migration: an older file (no `profiles`) becomes one profile on load (`"default"`,
  active in server mode, inactive in local mode with a saved URL); the original is copied
  once to `settings.json.pre-profiles`. When an older client later edits `mode`/`server_url`
  directly, those fields win on the next load (an unknown URL becomes a new profile).
  Migration is idempotent.
- Tokens are per profile in the same DPAPI `secrets.bin` (`profiles: {id: {token}}`);
  the old single token moves to the first profile, and the top-level `token` stays the
  active profile's for older clients. An older client that rewrites `secrets.bin` drops
  the map; the next start migrates its single token again.
- The 「音效库管理」 window (see *Desktop usage*) lists 本地素材库 and every profile
  and edits exactly the selected one; editing never switches the source. Save on a
  non-active profile only stores settings (no runtime reset); on the active profile a
  changed address, token or transcode choice saves through the switch path (runtime
  reset) and syncs again. An address already saved in another profile is refused
  ("这个地址已保存在服务器配置「…」中"). The token is never loaded into the window: the
  field stays empty (empty = keep, a typed token replaces it, 清除令牌 removes it on save).
  「使用服务器外网地址」 (from `/health` `public_url` after a test) creates or updates a
  sibling `external` profile in the same group (copying the token, not switching) and
  selects it. Deleting a profile removes it and its token only (never caches, snapshots or
  data directories); the active profile cannot be deleted.
- `last_test` is written by 测试连接 when the tested address is the profile's saved one
  (one line from `ConnectionReport::verdict`, plus the library id); a test of an unsaved
  address is shown but not recorded.
- `transcode` ("试听时转码（适合外网或低带宽）", per profile, see Playback → Route)
  decides whether auditions are transcoded. Defaults: off for `http` LAN addresses
  (private, loopback or link-local IPs, `.local`, single-label hosts) and new LAN
  profiles; on for `https` or public hosts and for the sibling created by "Use the
  external address". Migration fills a missing value from the address, except that the
  old global `audition_original: true` ("original quality") becomes off. The connection
  page keeps a profile's saved choice unless the box is clicked (an untouched box follows
  the address class being edited: LAN off, internet on). `audition_original` is still written as the active profile's
  inverse for older clients but never read back once every profile has a value.
  `AudioLibCli remote status` shows `active_profile.transcode` and `audition_transcode`;
  `remote profiles` lists `transcode` per profile.
- Switching the active source (「切换到此数据源」, or 「保存并切换」 with unsaved edits)
  saves settings and secrets together, resets the per-server runtime state and swaps
  through the normal path (local: `LibraryOperation::DataSource(Local)`; a profile: a sync that
  opens its snapshot). The snapshot cache is keyed by library id, so a LAN and an
  external profile of the same server share one cache.
- Local settings and switching back to the local library remain available while a
  server snapshot sync is pending, downloading or failing, including the sync after
  selecting a server. Switching cancels and invalidates the old sync; late progress,
  results and queued retries cannot restore that server selection. Metadata/settings
  writes and actual provider opening still drain before another source change, and
  abandoning a server selection preserves the original provider's rollback settings.
- Keyboard navigation also keeps the local recovery controls reachable. Saving an
  active connection on Close leaves the manager visible until the new provider
  commits; choosing another source cancels that pending Close. If local opening
  fails, rollback restores the original settings and waits for cancelled cache
  publication before restoring the active cache. A window-close request during
  that wait proceeds through the normal bounded shutdown drain; any unfinished
  cache marker is still checked against the configured endpoint on restart.

## Server address and HTTPS

The dialog accepts the address in two forms (toggle next to *Server address*):

- **Full URL**: `https://audiolib.example.com:8443`, `http://192.0.2.10:8790`, or with a
  path prefix `https://example.com/audiolib`. Input without `scheme://` means `https://`.
- **Domain + port**: scheme selector (`https://` default, `http://`), domain or IP, port
  (empty = 443/80) and an optional path prefix. Switching forms carries a valid address
  over.

`audiolib_core::server_url::ServerAddress` parses both: input is trimmed, the scheme is
lower-cased (host spelling is kept so legacy cache keys do not change), a trailing `/`
and a pasted `/api/v1` are dropped, and the result is stored as `server_url`. Hosts must be
DNS names (labels of 1–63 letters, digits, `-` or `_`, at most 253 characters), IPv4
addresses or bracketed IPv6 (a bare IPv6 in the domain field is bracketed); ports 1–65535;
the prefix may not contain `.`/`..` segments, whitespace, `?`, `#`, credentials or non-ASCII
text. Each problem has its own Chinese and English message, and a hint line under the
fields shows the API address that will be used (or the error) while typing.

**TLS.** Every client agent (API, snapshot, streams, previews, waveforms, downloads and
WebDAV) uses one `ureq` TLS configuration: rustls with ring and
`RootCerts::PlatformVerifier` (`rustls-platform-verifier`), i.e. certificate chains and
host names are verified by the operating system trust store (Windows certificate store,
including enterprise roots and roots Windows fetches on demand). Only publicly trusted
certificates are accepted: there is no self-signed exception and no way to disable
verification. Failures are classified (`RemoteError::Tls { kind, .. }`):

| `TlsFailure` | Message points to |
|---|---|
| `untrusted` | the server needs a publicly trusted certificate (e.g. Let's Encrypt); self-signed is not accepted |
| `hostname_mismatch` | the certificate does not cover the entered domain |
| `expired` / `not_yet_valid` | renew the certificate; check this PC's clock |
| `revoked` | replace the certificate |
| `handshake` | the port may only speak HTTP (use `http://`) or the reverse proxy is misconfigured |

DNS failures and refused connections are named as such (`RemoteError::network_failure`),
separately from timeouts.

**401.** A missing and a wrong token are both `401 UNAUTHORIZED` (API §2.2); the client
tells them apart by whether it sent a token (`TokenRequired` vs `Unauthorized`), never by
the message text. Both messages tell the user to open the server's web admin page
`http://<LAN server address>:8790/admin` from the local network, generate a token there and
enter it under *Data source settings → Access token*. The client never calls `/admin`.

**External address.** `/health` may carry `public_url` (API §6.2). When it differs from the
tested address (e.g. a LAN test), the connection report shows "Server external address: …"
and the dialog offers **Use external address**, which fills it into the URL field.

## WebDAV URL per root

`GET /roots` returns `{name, server_path, webdav_url|null, file_count|null}` for each scan
root. The client resolves an **effective URL** per root with this precedence:

1. **User override** — the URL the user entered for that root name
   (`webdav.root_urls[name]`, edited in the Data source dialog, one row per root).
2. **Server suggestion** — the root's `webdav_url` from `/roots`. The server value is only a
   default suggestion; the client override always wins.
3. **Global base URL** — `webdav.base_url + "/" + <root name>` (quick option when no
   per-root rows exist or a root has neither of the above).

An empty row, or a row equal to the server suggestion, is stored as "use the server
default" (no override). A record path is mapped by the longest matching `server_path`
prefix on a path-component boundary (exact case first, then ASCII case-insensitive); the
remaining components are percent-encoded and appended to the effective URL. The test
report shows each root's effective URL and its source (user / server / base URL).

## Sync and the snapshot cache

Layout under `<data dir>/remote/`:

| Path | Content |
|------|---------|
| `active.json` | Identity of the snapshot last opened in server mode |
| `<library_id>/cache.json` | `CacheState`: library id, revision, schema, file name, server URL/version, download/check times, size, record count, last roots |
| `<library_id>/library-r<revision>-<millis>.db` | Validated read-only snapshot; unused generated mirrors are pruned after a swap |
| `.transfers/<endpoint-sha256>/payload.part` | Durable HTTP representation bytes retained for snapshot resume; zstd stays compressed |
| `.transfers/<endpoint-sha256>/checkpoint.json` | Atomic transfer identity, encoding, lengths, durable offset and prefix SHA256; never contains credentials |
| `.staging-locks/<library-key>.lock` | Retained OS lock shared by snapshot preparation and conservative housekeeping |
| `placeholder/library.db` | Empty library in the current local schema shown before the first successful sync |
| `secrets.bin` | DPAPI-protected token and WebDAV password |

Snapshots use unique file names instead of a fixed `library.db` because Windows cannot
replace a SQLite file that a reader still has open. Only one snapshot stays on disk: the
previous file is pruned after the swap, and the poll loop retries while Windows still has
it open. A resumable transfer also retains the wire representation: disk use peaks at
the old validated database + the new decompressed database + the compressed spool
(identity responses can require a second database-sized wire file). Leave room for
SQLite validation and derived-state preparation as well. Insufficient space is a clear
error; it never replaces the old cache or deletes a source WAV. A sync
itself never deletes anything: the previous snapshot and replaced library directories are
removed only after the provider has swapped to the new snapshot
(`RemoteCache::cleanup_after_swap`; it never deletes the open snapshot or the one
`cache.json` names, and does nothing while a newer sync still waits for its swap). Known
top-level snapshot staging files (`.download`, `.download.part-<pid>`) and `cache.tmp-*`
older than an hour are checked across cached libraries at startup. After a successful
provider switch, cleanup checks the opened library and same-server replaced libraries.
Cleanup takes nonblocking staging/publication leases and rereads current state;
an active preparation or publication is skipped. Unknown files, linked paths,
manual backups and `.snapshot-job-*` protected recovery jobs are retained.
Recovery jobs also protect snapshots they may need. Cleanup does not descend into
these directories or prune resumable files under `.transfers/`:
paused or interrupted downloads survive application restart and are retained until
successful sync, explicit restart, library replacement or server snapshot invalidation.
Audition no longer writes to disk; the `audio-cache/` directories
of earlier versions are checked once (`remove_audition_caches`, at startup in server mode
and on a server switch). Only their generated flat-file namespace is removed;
unknown entries and linked paths are retained.

**Replaced libraries.** When a sync of a server URL installs library B,
`RemoteCache::discard_replaced` checks other libraries whose `cache.json` names the
same server URL, also on polling and source switches. Under the same nonblocking
leases it rechecks the active library and recorded state, then removes only
identified generated mirrors and stale temporary files. A recovery job preserves
the whole candidate library. Manual backups, unknown files and directories keep
their directory/state available; no recursive cleanup is performed. Empty generated
library directories can be removed, with open-file failures retried later. Caches of other servers are
kept, so switching between servers does not re-download. The drag-out download cache
(`<data dir>/cache/<library_id>/`) is not purged: it is one 4 GiB LRU across libraries, so
files of the old library are evicted normally as new ones arrive. A new library never
shows rows of the old one: its snapshot lives in a new directory and is downloaded from
scratch.

**Open the mirror directory.** Server Settings → Cache and export shows an
**Open mirror folder** button beside the selected server's mirror information.
It requires an existing snapshot with matching library/endpoint identity and an
existing cache directory. A saved LAN/external sibling can share the mirror only
when both profiles have the same known library ID and belong to the same group.
The action rechecks this identity before opening Explorer;
it creates no directory. A validated existing mirror can still be opened while a
new snapshot is syncing. The cleanup note distinguishes automatic old-mirror/temp
cleanup from retained resumable downloads, recovery backups and manual backups.

Sync runs at startup in server mode, after switching to a server profile (or saving a
changed connection of the active one), on "Sync now" and on Refresh (F5). Steps:

1. `GET /health` (no auth) and `GET /library` (Bearer). A schema newer than this client
   supports is rejected with a clear message.
2. `GET /roots`; on failure other than 401 the cached roots are kept.
3. If a cache exists with the same `library_id` and `revision`, only `checked_at` is
   updated (`AudioLibCli remote sync --force` re-downloads anyway).
4. Otherwise `GET /library/snapshot` streams to an endpoint-specific durable wire
   spool and simultaneously decodes into an owned `.download` file (never buffers the
   whole snapshot in memory). The wire bytes are written before the decoder consumes
   them. An atomic checkpoint records a flushed prefix at most every 8 MiB or 2 s,
   and on an orderly pause/interruption. Content-Length is checked; headers and body
   reads remain cancellable. The desktop shows "The server is preparing the
   snapshot…" until the first byte (the server copies the library first: about 7 s for
   645 MB). The connection and cache/export pages then show separate download and
   decompression rows and progress bars for zstd snapshots. Download bytes count the
   compressed HTTP body against `Content-Length`; decompression counts database bytes
   written against `X-Uncompressed-Length`. These operations overlap while streaming:
   download can reach 100% while decompression continues. A 19,495,989,248-byte database
   therefore shows 18592.8 MiB as the decompression total, never as the compressed
   download total. On resume, already downloaded wire bytes stay in the download
   counter. The incomplete decoded database is rebuilt by replaying the local prefix
   through one decoder and then reading the resumed HTTP suffix; this costs local
   decompression time but does not download that prefix again. Replayed bytes do not
   count toward network throughput. Either missing total keeps only its own bar indeterminate; identity
   responses show download only. Both counters are retained across coalesced GUI updates
   (at most every 200 ms, except stage appearances and completion), and validation/cache
   preparation replace the transfer rows when streaming finishes. `409 BUSY` and `503 BUSY` are retried for up to 240 s from the first request (`BUSY_WINDOW`), waiting the server's
   `Retry-After` when sent (capped at 10 s per wait), else exponential backoff (0.5 s, 1 s, 2 s, 4 s, then 10 s). An anonymized large-library measurement took the server about 125 s to prepare its first snapshot and its first HTTP response was a `503 BUSY` with `Retry-After` after about 90 s; the 240 s window (and the 240 s snapshot response timeout) covers that measurement with margin, so the client does not fail early.
5. The `X-Library-Id` header must match `/library`; the revision comes from
   `X-Library-Revision` (falls back to `/library`).
6. Validation: `journal_mode=DELETE`, `user_version` equals the supported schema, opens
   read-only, and `SqliteProvider::check_owned_file` checks the downloaded file in place:
   `quick_check`, foreign keys, FTS5 `integrity-check` and the expected triggers.
   `check_library` makes a full temporary copy because a live library can be written by
   another process. A downloaded snapshot belongs only to the sync, so it needs no copy.
   That saves a second copy on disk (in `%TEMP%`) and a second pass. On a 645 MB snapshot
   validation dropped from 9.2 s to 1.4 s.
7. Atomic rename to the final file, `cache.json`/`active.json` update, provider swap
   through the existing library-operation machinery (metadata FIFO drained, reader and UI
   generations invalidated, selection and drafts cleared).

### Interrupted downloads and resume

This change extends the `0.6.0` main baseline and retains its shared format catalog
and protected migration rules. Detect resume support from the response headers
below; application version alone does not establish protocol or file compatibility.

A resumable server reply provides a strong representation-specific `ETag`,
`X-Snapshot-Id`, identity/schema/revision headers, `Accept-Ranges: bytes` and a known
wire length. The client saves these with its durable prefix and hashes that prefix
again before reuse. A transfer lease excludes another desktop/CLI writer for the same
normalized API endpoint. Profiles with different endpoints never append to one
another's partial file; neither tokens nor WebDAV credentials enter the checkpoint.

After a disconnect or 30 s with no network read progress, the client retries up to
three times after 2, 5 and 10 s. It requests the same immutable snapshot using
`snapshot_id`, `Range: bytes=<offset>-` and `If-Range`. Only a matching `206` whose
Content-Range, ETag, encoding, identity and lengths agree can append. A full `200`
response cannot be appended to a prefix. Expired/missing snapshots, a replaced
library or a changed representation start a fresh safe transfer; the previous
validated cache remains available. A complete wire spool can finish local decoding
and validation without downloading the snapshot again. Older servers lacking the
resume headers still support a full download, with retry starting from zero.

The server retains immutable representations for at most 24 h of inactivity in the
current process. The registry is bounded to 4 representations and 64 GiB; capacity
pressure may evict inactive entries sooner. An active response lease prevents its
files being deleted while streaming. A known ID that expired, was evicted, belongs
to a replaced library or disappeared after a server restart returns `410`; clients
discard that checkpoint and safely start a new snapshot. Resume therefore does not
guarantee that a server restart or 24-hour interruption can reuse the old bytes.

The desktop exposes Pause/Continue and Restart Download while retaining separate
download/decompression progress. Manual pause persists and suppresses background
automatic sync, including startup and revision polling, until explicit continuation.
Closing the application cancels its socket, saves the durable checkpoint and drains
the worker; the next startup can resume. Pausing a first sync leaves a matching
validated cache or the empty placeholder usable. No partially decoded database is
published or opened as the library.

Wire files remain until the validated cache has been published and its provider
has opened successfully. Cleanup acknowledges the exact published database file
under the endpoint lease, so a late acknowledgement cannot remove a newer transfer
or pause. CLI sync acknowledges its validated publication before returning.

`AudioLibCli remote sync` resumes a compatible interrupted transfer automatically.
`--force` still forces a sync when the active cache already has that revision;
`--restart` explicitly discards the pending transfer and starts at byte zero.
Ctrl+C preserves a resumable prefix. Retrying exhausted network failures in a later
CLI invocation uses the same checkpoint. A persistently paused GUI transfer is
resumed only by an explicit user sync/continue action.

A changed `library_id` (server database copied to a new location) shows a warning and
resyncs; an in-place restore keeps the id and bumps the revision. If the server is
unreachable at startup, the last cache opens offline; without any cache an empty
placeholder library is shown with a banner, and the next successful sync replaces it.

The server-mode GUI opens on a small placeholder and displays the library manager.
Its single sync worker checks the server revision before selecting a snapshot: reuse
the current cache, prepare an older cache in an owned copy, or download a newer snapshot.
It does not first upgrade the old snapshot in a separate startup window. If the server
is unavailable, the same preparation path opens the last cache matching the configured
endpoint. The local working library stays untouched. Headless validation still opens
the cached library synchronously.

A rejected new download or unsupported server schema also preserves access to a valid
matching old cache, with the original failure still visible. A failed preparation of
that old cache is not immediately attempted again in the same sync.

The manager shows the actual copy/download fraction, bilingual preparation stages and
elapsed time. Search-text upgrades report completed records in batches of 5,000,
followed by separate FTS5 construction and extracted-tag stages. These counts describe
the text stage only; integrity checks and FTS5 construction keep an animated bar
without an estimated percentage. Owned upgrades use a 64 MiB SQLite page cache and
memory-backed temporary work, retaining rollback-journal mode and its durability.
All text batches and the FTS5 rebuild share one transaction. Cancellation before its
commit rolls back that transaction; cancellation later in preparation discards only
the owned staging copy. The original cache and its readers remain available.
Stage transitions, bounded five-second text-progress logs and upgrade timings help
distinguish a large rebuild from a stalled worker without logging Metadata or secrets.
The source-change gate preserves drafts while
leaving this progress visible through the final provider-open stage. Closing drains
the cache worker before releasing the desktop instance lease. Original integrity
checks and owned-copy safety are retained;
an unchanged prepared snapshot is reused without copying, downloading or rebuilding.

Timeouts: connect 3 s, ordinary JSON responses 15 s (bodies 30 s), library statistics
response/body 60 s, root path PATCH and root DELETE responses up to 1 h (bodies 30 s),
snapshot response 240 s and a 30 s network-read stall limit (no one-hour total
transfer deadline), audio stream first byte 100 s (a NAS disk spinning up, see *Playback*). The stall limit excludes local prefix replay, decompression and disk work. Errors map to localized messages for unreachable server, timeout, 401
(invalid token, or "token required" when none is configured), 404, BUSY, validation and
server `INTERNAL` errors. Root PATCH/DELETE have no generic server 30 s request limit.
A server `408 TIMEOUT` (a request exceeding its applicable server time limit) is treated exactly like a client timeout: transient, shown as
offline in polling (the cache stays in use and the check is retried), retried by the audio
stream after its first response, never reported as a token or protocol problem.

Sync health/library/roots checks and snapshot transfers use a scoped cancellation
transport. Cancel closes the active socket, and scoped reads poll at most every 100 ms
so a Windows blocked read also exits while a peer remains silent. Snapshot cancellation
returns after its durable resume checkpoint is saved and the cancellation watcher
joins; existing cache state and validated snapshot bytes are retained. Incomplete
decoded output is disposable and rebuilt on resume. Owned SQLite validation/upgrades
still drain safely.

## Revision polling

In server mode a background thread (`audiolib-remote-poll`, never the UI thread) calls
`GET /library` every `poll_interval_secs` and compares `library_id` and `revision` with
the open snapshot (`RevisionCheck::classify`, unit tested):

| Result | Header label | Notice under the header |
|---|---|---|
| same id and revision | — | hidden |
| new revision | `· update r<N>` | "Server library updated: revision N" + **Sync** |
| different `library_id` | `· library replaced` | identity change warning + **Sync** |
| no cache yet, server reachable | — | "not synced yet" + **Sync** |
| unreachable / timeout | `· offline` | "Server offline: using the local cache" + retry time + **Retry now** |
| `503` (library still opening) | `· server starting` | "The server is starting" + retry time + **Retry now** |
| `401` | — | token rejected (or "server requires a token" when none is saved) + **Settings…** |

With `auto_sync` an available update starts the normal sync (same validation and provider
swap as *Sync now*) once it has **settled** (`AutoSyncGate`): a new revision is synced once it has stayed the same for at
least one poll interval (an extra check soon after, such as **Retry now** or saving the
settings, does not count), or after at most 15 minutes
(`AUTO_SYNC_MAX_DEFER`) of continuous change. A server that bumps the revision every few
seconds (batched scan) therefore does not trigger one full 600 MB download per batch. While
deferred, the notice says the server is still updating and **Sync** still syncs at once. A
changed `library_id` or a first sync is never deferred. The notice is non-blocking and can be dismissed (×); it stays hidden
until the situation changes. The schedule (`PollSchedule`): one interval after a successful
check; while offline the delay doubles from the interval up to 15 minutes; a `503` is
retried after 10, 20, 40 s… (at most the interval, never sooner than the server's
`Retry-After`); a `401` waits at least 5 minutes. A sync that fails with `503` schedules a
re-check after 10 s or `Retry-After`, whichever is longer. Saving the Data source
settings, **Retry now** and the notice re-check immediately; F5 / *Sync now* sync as
before. Checks are skipped while a sync or library operation runs, and a result for a
snapshot that was replaced meanwhile is discarded. Window-focus checks are not
implemented (Slint 1.17 exposes no focus-change callback without unstable winit APIs).

`503` (`UNAVAILABLE`, with `Retry-After: 5`) is reported as "server starting, retry later"
everywhere (sync, connection test, polling), never as an authentication failure. The
server checks the token first, so a wrong token is still `401` while it starts. `/health`
always answers `200` with `status` `ok|starting|error`; the connection test shows
`starting`/`error` as a distinct health step (`…`/×) and still runs the library step so
the token is verified.

`BUSY` is never shown as "server starting" or as a token problem, whatever its status:
`409 BUSY` / `503 BUSY` while downloading a snapshot (slots, or a large first snapshot still being generated) wait Retry-After (cap 10 s) when sent, else the sync backoff of 0.5/1/2/4/10 s, for up to 240 s in total; a `503 BUSY` on the audio stream (stream cap)
is retried by the audio stream (see *Playback*), and a `BUSY` reaching polling or the
connection test is reported as "server busy" (notice with **Retry now**). The error keeps
the server's `Retry-After` (`RemoteError::Busy { retry_after }`).

## Scan root status (§6.4)

Each poll that reaches the server also fetches `GET /roots` (cheap: the server answers from
its root status cache and never touches the NAS). Each root carries `online`, `status`
(`checking|online|waking|offline`), `status_message` and `checked_at`. Missing fields mean
online, and the older `available`/`reachable` names are accepted as `online`
(`RemoteRoot::state()`).

| State | Header | Notice / status |
|---|---|---|
| all online | — | — |
| `waking` (no answer from the disk for > 5 s) | `· disk waking: <root>` | no notice; playback of its records shows the waking hint at once |
| `offline` | `· root offline: <root>` | offline roots with their reason; their records stay listed (not missing), cached copies still play (shown when no revision notice is pending) |

The Data source dialog lists each root with its status (warning color when offline), and
`remote test` lists `N roots: name status; …`. An offline root does **not** mark its
records as missing (the server never reconciles files of an unreachable root): rows keep
their normal appearance, and the root status explains why they cannot play.

**Retry** on the offline-root notice calls `POST /roots/{name}/check` for each offline root
(the server answers within about 2 s) on a worker thread and replaces those entries in the
live root status; the notice stays while a root is still offline and the status bar says
which. A server without the endpoint (`404`) gets a `HEAD /audio/{id}/stream` of the
selected record instead, when it lies in that root. Switching to local mode, falling back
to the offline cache or saving the settings drops the live root status (saving also wakes
the poller; a successful sync brings fresh status); until then the roots saved with the
snapshot are used without their sync-time status.

Playback of a record under an offline root: the server answers `503 ROOT_OFFLINE` +
`Retry-After: 5` after a bounded inline recheck (`RemoteError::RootOffline`, classified by
`error_code`; an older server's `UNAVAILABLE` naming a root is still recognised). With a
WebDAV URL configured for the root, playback falls back to it (and tries it first while
`/roots` reports the root offline). Otherwise, before the first
response this is shown at once, with no retry loop: "Library root offline, cannot audition
now: <server message> (cached sounds still play)". The poller is woken so the header
refreshes. After the first response it counts as a transient error (3 bounded retries,
like `503`). A `404` means the file is missing on a reachable root: "the server cannot
find this file in its root <name>: moved, renamed or deleted". `remote download` and
drag-out report the same message.

## Writes in server mode

From 0.4.4, each writer binds the opened snapshot's immutable library ID. Before every
mutation it checks `/library`, requires `identity_bound_writes: true`, and sends
`X-AudioLib-Library-Id`; the server compares that value against the same captured
AppState/provider used by the handler. This closes the gap between checking and writing.
Older servers remain readable but bound writes are refused until the server is upgraded;
older clients without the header cannot write to the upgraded server.

A known replacement disables GUI editing and management until the replacement snapshot
is opened. Offline checks cannot re-enable stale IDs, and delayed events from a former
library/snapshot cannot disable the new provider. Identity rejection leaves drafts and
cached bytes intact and is distinct from authentication, busy and uncertain write errors.

When a confirmed replacement prevents saving old drafts, switching sources, opening the
replacement snapshot or closing first drains requested writes and preserves those drafts
in `<data dir>/recovery/server-metadata/*.json`. Each recovery file binds the original
library ID and snapshot, WAV paths, Metadata patches and failed quick actions. It is
published only after its contents are flushed; a read or write failure keeps the old
provider and drafts available for retry. Failed target opening also retains the in-memory
drafts. The UI reports the recovery location and surfaces it again after restart.
Recovery files are outside the pruned snapshot/audio cache, remain local, contain no
credentials, and are never automatically imported or uploaded. Review their annotations
against the original library and WAV paths; record IDs must never be applied to a
replacement library. Ordinary network or validation save failures still block handoff
and close until corrected; they do not activate this recovery exception.

Annotations and permitted management changes are sent to the server; the opened
snapshot remains a read-only search provider.

**Allowed (sent to the server).** Inspector edits (favorite, tags, color including
clearing it, notes), quick edits from the context menu and the menu bar, and
batch edits of the checked records (tags *replace* or *append*; the API's *remove* mode is
available in the CLI). The desktop routes these through `DataProvider` like in local mode:
in server mode the Metadata worker and the batch operation use
`audiolib_remote::RemoteProvider`, whose reads come from the snapshot and whose
`update_metadata` and `update_metadata_batch` call the endpoints below. Playback
and play history are local no-ops: they send no HTTP request and write no cache data.

| Operation | Request | Response |
|---|---|---|
| single edit | `PATCH /api/v1/audio/{id}/metadata` (JSON patch, absent fields unchanged, `"color":""` clears) | `{file, revision_before, revision}` |
| batch edit | `POST /api/v1/audio/metadata/batch` `{ids, patch, tag_mode}` (one transaction) | `{report:{updated}, files, revision_before, revision}` — `files` in request order, duplicates removed |

**Revision rule.** After a successful write, if the cached snapshot's revision equals
`revision_before`, the returned record(s) are written into the open snapshot in one SQLite
transaction (`audiolib_db::apply_snapshot_records`: tags/notes, favorite and color,
with FTS5 maintained atomically). Legacy schema 3–5 keeps its derived columns and
stored count; schema 6 derives the tokens at index writes and ignores retired counters.
The cached revision becomes `revision` (`cache.json` and the header); a valid response
with `revision == revision_before` also applies without requesting full sync. Otherwise (another
client wrote in between, no cached state, a different snapshot open) a normal full
snapshot sync runs. The background revision poll therefore does not report the client's
own writes as "new revision".

**Errors.** Writes are never retried automatically and errors never change the cache:

| Server answer | Client message / behaviour |
|---|---|
| `400 VALIDATION` (color not `""`/`#RRGGBB`, tags > 16 KiB, notes > 1 MiB) | "invalid value" with the server's text (also checked locally before sending) |
| `404 NOT_FOUND` | the record no longer exists on the server (a batch is rolled back) — sync |
| `409 BUSY` | the server is scanning; try again shortly |
| `413 PAYLOAD_TOO_LARGE` | request too large (server limit 8 MiB; the client refuses bodies over 8 MiB before sending) |
| `401` | token required / rejected, as for reads |
| `408 TIMEOUT`, timeout, connection lost, unreadable response | outcome uncertain: shown as an error **and** a full sync runs |

A failed inspector save keeps the draft (shown as unsaved) like a failed local save.
Snapshot handoff requires successful persistence, except for the confirmed replacement
recovery described above.

**Refused (still read-only).** Local scan workflows, maintenance and restore, individual
file relinking, legacy import and annotation import, new/open local library and
reveal in Explorer. Remote scans, removals and root path changes use the authorized
server-management workflow below. The UI disables unavailable local actions with the suffix "(read-only in server mode)" /
"（服务器模式只读）" (Slint 1.17 has no tooltip widget), the library dialog shows an
explanatory line, and the controller refuses them independently of the UI
(`refuse_remote_write`; `RemoteProvider` also refuses local-only operations as a `DataProvider`). The
inspector shows "Server mode: … saved to the server (source WAV unchanged)" and the save
label reads "Saved to server". Source WAV files are never modified.

**Plays.** When the playback history counts a play of a server record (the same rules as
local mode), the client posts `POST /audio/{id}/plays` on a short-lived worker thread
(fire-and-forget; errors are logged, never shown) and applies the revision rule. The
record is identified by the playback cache path it was played from. Nothing of this runs
on the audio thread.
## Server management (scans, history, removals)

`audiolib_remote::RemoteClient` has typed calls for API §6.6–§6.10; the CLI exposes the
read-only ones. The desktop's 服务器音效库 page (design B1/B2) uses `roots`,
`capabilities`, `current_scan` (polled every 500 ms while the page is open and a scan is
active) and `scan_history(20)` with the profile's saved address and token. For the
**active** server, and only when `capabilities.may_manage()` (`can_manage`), it also
starts scans (`start_scan`: chosen root, optional relative subpath, incremental or full,
workers left to the server), pauses, resumes and cancels the active scan, and removes
checked records or the selected folder's records (`RemoteProvider`, so a revision bump
re-syncs the snapshot and cached ids are dropped). `409 BUSY` / `SCAN_STATE`, `503
ROOT_OFFLINE` (with `Retry-After`) and `403 MANAGE_FORBIDDEN` are shown with the
server's text; without `can_manage` the controls stay disabled and the page shows the
server's mode.

The existing removal actions of the main window (Edit → 移除所选记录 / 移除选择记录 /
移除当前目录, the result and folder context menus, the list's 移除 button) work in server
mode too when the active server's `capabilities.may_manage()`: they ask with the same
wording as the page and make the same calls through `RemoteProvider`. The answer is
fetched after every sync and belongs to the server it came from. Otherwise the items stay
greyed with the server's reason (for example `manage_mode = lan`), and the controller
refuses them as well.

After a 200 the removed records disappear at once (optimistic overlay): the controller
keeps the removed ids and the removed folder's subtree (read from the snapshot before the
call) in memory and filters result pages, folder matches and the record count; the
selection is cleared and only the removed ids' audition-stream and waveform entries are
dropped (a folder removal drops all of them, its ids being unknown). The snapshot file is
never modified. The revision-bump resync then opens a snapshot past the removal and the
overlay goes: from then on the snapshot wins. If that resync never comes, a restart shows
the snapshot again. While the overlay is active a page's total only loses the hidden rows
of that page, and folder counts stay as in the snapshot.

| Call | Request | Notes |
|---|---|---|
| `start_scan` | `POST /scans {root, subpath?, mode, workers?}` | subpath relative, normalised and checked locally (no `.`/`..`, `\`, leading `/`); workers 1–16 (0/None = auto); 120 s client timeout, on timeout check `current_scan` |
| `current_scan` | `GET /scans/current` → `{"scan": … \| null}` | attach on (re)connect |
| `scan_status` | `GET /scans/{id}` | poll about every 500 ms until `active` is false; a changed `revision` means sync |
| `control_scan` | `POST /scans/{id}/pause\|resume\|cancel` | idempotent; `409 SCAN_STATE` → `RemoteError::ScanState` |
| `scan_history` | `GET /history/scans?limit=&offset=` | newest first, limit clamped to 1–10000 |
| Recent playback | client-local `play-history/<library_id>/history.db` | no playback HTTP calls; independent of snapshot/cache cleanup |
| `capabilities` | `GET /capabilities` → `{manage_mode, external, can_manage, root_management}` | `may_manage()` = `can_manage` (never inferred locally); missing `root_management` disables root editing; `404` (older server) → no management |
| `browse_directory` | `GET /fs` or `GET /fs?path=&hidden=0` | no path returns server locations; absolute server path returns one bounded, non-recursive directory listing |
| `add_root` | `POST /roots {name,path}` | register an existing server folder; no implicit scan |
| `update_root_path` | `PATCH /roots/{name} {path}` | validate all matching target WAV files before an atomic database path update; retains record IDs and annotations |
| `remove_root` | `DELETE /roots/{name}` | remove root configuration and database records/annotations; preserve folders and WAV files |
| `remove_audio` | `POST /audio/remove {ids}` → `{removed, not_found, revision_before, revision}` | ids de-duplicated, 1–10000 positive; DB records only |
| `remove_folder` | `DELETE /folders/{id}` → `{removed, folders_removed, revision_before, revision}` | whole subtree, DB records only; `404` unknown, `400` for a root's top folder (or one containing a root) |

`/roots` entries carry `missing_count`, `scanning` and `last_scan` (Unix-second times,
`state`). Typed errors with Chinese text (`manage_error_text`): `403 MANAGE_FORBIDDEN` →
`Forbidden`, `409 BUSY` → `Busy` (any other 409 too), `409 SCAN_STATE` → `ScanState`,
`503 ROOT_OFFLINE` → `RootOffline`; a server without the endpoint (`404 "No such endpoint"`
or `405`) → `Unsupported`. The `403` text names the reason (LAN only / disabled) and
includes the server's message. `manage_mode` gates scan control, removals, remote disk
browsing and root management; Metadata edits and play counts keep their existing policy.

Removals make the library forget records; a rescan re-adds files still on disk with new
ids. Both are `409 BUSY` while a scan runs or is paused. `RemoteProvider::remove_audio` /
`remove_folder` resync the snapshot when the revision moved (a folder removal always
moves it, an audio removal only if something existed); a UI built on them must also drop
cached ids (selection, waveform/preview keys). Root configuration and resource path changes
use the explicit server endpoints above; the client snapshot remains read-only. Restore,
maintenance and annotation import remain unavailable in server mode.

Root relocation and removal create a rollback backup before changing database records.
Durable journals reconcile database and root-settings changes after an interruption;
the server recovers them before accepting another root edit. See the API contract
[§6.10](api/README.md) for validation, recovery and cancellation details.

The management calls go through the same keep-alive connection pool as everything else
(see "Connections" below): reads (`GET`) are sent once more on a stale pooled connection,
writes (`POST`, `PATCH`, `DELETE`) never are. A lost mutation response can mean the server
committed the operation; the desktop refreshes its snapshot rather than resending it.

## Playback (in-memory streaming)

Audition never writes to disk. `audiolib_remote::MemoryStream` fetches the selected
record into a process-wide `MemoryCache` and the engine reads it through a registered
byte source; the drag-out still copies the original from the download cache.
Each new byte-source registration receives a distinct virtual path even when WebDAV and
the server use the same raw route. A failed source-pair change can therefore restore the
old registration; successful handoff removes only the superseded registration and keeps
any shared cache entry alive.

1. **Route** (`route::choose_route`, §8.1). Transcoding is the user's choice per server
   profile ("试听时转码（适合外网或低带宽）" on the connection page,
   `profiles[].transcode`, see "Server profiles"); the client setting is authoritative
   and the measured throughput never overrides it. **Off**: every audition reads the
   original `/audio/{id}/stream` (Range, memory LRU and the buffering state as usual);
   `/preview` is never requested. **On**: WAV uses
   `/audio/{id}/preview?codec=pcm&sr=48000&bits=16` (`sr=44100` for the 44.1 kHz family,
   never above the source; `ch=2` for more than two channels while the first source pair
   plays); non-WAV files still use `/stream`. `sr` and `bits` are always sent:
   `codec=pcm` alone keeps the original 96k/24. Below 2.5 Mbit/s the
   preview is `codec=flac` (same `sr`, 16-bit, stereo; mono below 0.8 Mbit/s). Fallbacks:
   `415` on pcm → flac; flac refused (`415`, `503 TRANSCODER_UNAVAILABLE`) → `/stream`; a
   server without `/preview` (`404`/`400`) → `/stream`; `503 BUSY` is retried after
   `Retry-After`. Refusals are remembered per record (and "no transcoder" per server) until
   the server changes.
2. **Throughput** (`bandwidth::Bandwidth`). Exponential moving average (α 0.35) of
   transfer segments of at least 256 KiB and 0.5 s from streams, previews, FLAC and the
   snapshot; pauses (window full) are excluded. Before the first sample it assumes
   4 Mbit/s (a VPN or slow Wi-Fi looks like a LAN too); reset on a server change.
3. **Memory cache** (`stream::MemoryCache`). One budget (default 512 MiB,
   `data_source.audition_memory_mb`, 32–8192), 256 KiB chunks, whole-entry LRU over
   entries nobody holds or reads. Key = library + record id + file size + route tag +
   source (`server`/`webdav`) + snapshot filename. A newly synced snapshot invalidates
   complete audio after a same-size WAV replacement; ordinary annotation/play updates
   retain the filename and can reuse audio. The current-stream shortcut also compares
   the actual StreamSource (URL/auth) and route. A file of at most 64 MiB is fetched whole at link speed;
   a larger one up to about 60 s ahead of the player (at most 3/4 of the window = half the
   budget; 1/4 kept behind). Beyond that the fetch pauses (TCP backpressure; the response
   is held 15 s, then closed and resumed with `Range` when needed). A reused entry fetches
   only its missing parts with `If-Range` (strong ETag); a different version replaces the
   entry when nobody reads it and fails with `Changed` otherwise (a selection restarts
   once). The pcm ETag is opaque (`"<src>-pcm2-…"` today). Each read that waits more than
   30 s for data fails (per-read limit in the transport; there is no total body limit).
4. **FLAC previews** are decoded with Symphonia on the fetch thread into a synthetic
   16/24-bit WAV (header + total frames, so the engine stays WAV-only). The total comes
   from the first response's `X-Total-Frames` (else `X-Duration`) plus `X-Start-Frame`
   (server 0.3.6 counts them at the output rate from the start position); older servers
   use the record's duration. A seek
   asks for `t=` one frame early and aligns with `X-Start-Frame`; the gap before it and a
   short tail are silence; `416` past the end is silence. The weak FLAC ETag
   (`W/"…-flac1-…"`, varies with `t=`) is ignored.
5. **Engine.** The decoder decodes the next block only when it is in memory
   (`SourceBuffer::is_buffered`), so the engine worker never waits for the network:
   status, Pause, Seek, Load and volume keep flowing. Start and seek prebuffer 0.5 s (or
   the rest of the file). When the ring runs dry the gain ramps to zero in 12 ms, output
   pauses (`PlaybackStatus::buffering`) and resumes with a ramp once 2 s (or the rest) is
   buffered: one clean pause instead of per-frame silence. The status bar shows
   "Buffering audio…" after 350 ms. The playback header parse stops at the `data` chunk
   (it used to walk to the file's end, fetching the tail over a third connection) and Load
   opens the source once.
6. **Seek.** A wanted offset not in memory starts a new request at once (`Range` for
   pcm/raw, `t=` for flac) unless it is within about 0.3 s of transfer (64 KiB–1 MiB) of
   the byte being received. Duration and position always show the original record (a
   preview has the same seconds).
7. **Selections.** In server mode a selection opens its request and engine Load only
   after resting 180 ms (`SELECTION_SETTLE`), so arrowing through rows costs nothing; a
   new selection cancels the previous stream (its socket is shut down, the server frees
   the slot). Connections are registered by sharing the transport's own socket: a
   `try_clone` duplicate on Windows could report "not connected" right after connect, so
   its shutdown sent nothing and abandoned requests stayed open on the server until a
   timeout (found by the connection-count test under load).
8. **First byte and errors.** The first response may take up to 100 s (a waking NAS
   disk; hint after 1.5 s); Stop or another selection cancels at once. `503 ROOT_OFFLINE`
   before the first response is reported immediately (no retry loop); after it,
   connection loss, timeouts and `503` are retried 3 times (0.5/1/2 s, at least
   `Retry-After`). With a user-configured WebDAV URL for the root a failed server stream
   falls back to WebDAV (original file); while `/roots` reports the root offline WebDAV is
   tried first.
9. **Waveform.** `GET /audio/{id}/waveform?buckets=<segment columns>&colors=1&start_frame=&end_frame=`
   with `Accept-Encoding: zstd` and `If-None-Match`, drawn independently of the audio:
   memory (per revision) → disk `cache/waveforms/server/<library>/<key>.json` + `.etag`
   (revalidated; `304` reuses it). The first uncached range yields to the audio prebuffer
   for at most 3 s, once per viewport job. Start with about 0.5 s (rounded up to a display
   column), then grow batches up to about 1/16 of the viewport to bound long-file round
   trips. Publish the first partial immediately, subsequent partials at most every 500 ms,
   and completion immediately. Empty columns retain their source timeline position;
   the info line shows progress. Slow requests can exceed the refresh interval. Selection
   and viewport changes cancel requests and invalidate queued UI publications. A failure
   keeps the visible partial and reports the error. Only when the server has none and the
   audition is already complete in memory is a range computed locally. Independent FFT
   windows can give slightly different colors at segment boundaries.
   Validate source-frame boundaries, sample rate and peak structure before caching a
   server response. A damaged disk copy is refetched without its conditional ETag;
   rejected replies do not poison retries in the same revision. A downsampled audition
   tail shorter than one preview frame leaves its source column empty.

10. **Connections.** Every request (API, snapshot, waveform, audition streams, drag-out
   downloads, WebDAV) goes through one process-wide keep-alive agent (HTTP/1.1; up to 4
   idle connections per server, kept 30 s and probed before reuse). A completed response
   returns its connection to the pool, so the next selection or waveform request skips the
   TCP + TLS handshake (~0.5 s to first byte through a remote reverse proxy at ~175 ms
   RTT). Cancels are per request: a request made inside an `Exchange` scope registers the
   connection it currently uses with that scope's `Abort`; the registration ends with the
   scope, so cancelling a finished request never closes the idle connection it left in
   the pool. A cancelled or abandoned (seek) response closes its connection; the client
   keeps a spare ready in the background (`GET /api/v1/health`, at most two at a time) when an
   audition stream starts or is cancelled, so the next request finds a warm connection. The first byte may still take 100 s (NAS wake-up);
   body reads fail after 30 s without data (tolerates jitter on a remote link).

### Measurements (2026-10-09, anonymized server workload)

`cargo run --release -p audiolib-remote --example stream_bench` through
`tools/throttle_proxy.py` (GET/HEAD only, one shared token bucket, keep-alive client
connections). Throttled tiers add one round trip per request and two more for every new
connection (TCP + TLS): 40 ms RTT, and 175 ms RTT like the external reverse proxy (a new
connection costs ~0.5 s to first byte there). The bucket setting is not the goodput:
Windows timer granularity in the proxy makes "4" and "3" Mbit/s deliver ~2.8, "5" ~4.2,
"10" ~8.4, "2" ~1.7 and "60" ~34 Mbit/s (the client's own estimate, in brackets).
Samples: A (96 kHz/24-bit, 53 s), B (192 kHz/24-bit), C (96 kHz/24-bit 6ch); source record
IDs and server identity are omitted. Each sample used 20 s of
simulated engine playback each (0.5 s ring, prebuffer 0.5 s, rebuffer 2 s); then the record
selected afresh (nothing cached) and clicked to 70 % at the first sound ("seek"; at 175 ms
also 1 s after the first sound). Arrowing: 20 records at 120 ms and 300 ms per key
("arrow": first sound after the last key, including the 180 ms settle). The first record of
each tier starts with no bandwidth estimate (the app normally has one from the snapshot
sync) and with no pooled connection.

| Link [goodput] | Route A / B / C | TTFA ms | Stalls | Seek ms | Waveform ms (304) | Arrow 120 / 300 ms | Peak cache | 503 / max audio conns / audio reqs reusing a connection |
|---|---|---|---|---|---|---|---|---|
| LAN | pcm¹ / raw / raw | 30 / 17 / 18 | 0 | 5–7 | 8–13 (2) | 0.21 / 0.20 s | 294 MiB | 0 / 2 / 31 of 33 |
| 10 [8.4] @40 ms | pcm 48k/16 ×3 | 233 / 153 / 152 | 0 | 155–156 | 222–314 (47–54) | 0.33 / 0.39 s | 14 MiB | 0 / 2 / 28 of 33 |
| 5 [4.2] @40 ms | pcm 48k/16 ×3 | 324 / 226 / 232 | 0 | 276–277 | 286–486 (48) | 0.42 / 0.45 s | 12 MiB | 0 / 2 / 29 of 31 |
| 4 [2.8] @40 ms | pcm 48k/16 ×3 | 388 / 292 / 296 | 0 | 415–419 | 371–643 (48–50) | 0.48 / 0.57 s | 10 MiB | 0 / 2 / 27 of 31 |
| 4 [1.4 share] @40 ms + drag-out | pcm¹ / flac / flac | 726 / 409 / 389 | 1 (2.1 s, A)² | 461–541 | 751–1378 (90–107) | 0.80 / 0.67 s | 12 MiB | 0 / 2 / 31 of 33 |
| 2 [1.7] @40 ms | pcm¹ / flac / flac | 541 / 398 / 356 | 0 | 381–463 | 578–1074 (47–49) | 0.65 / 0.61 s | 12 MiB | 0 / 2 / 31 of 33 |
| 60 [34] @175 ms | pcm¹ / raw / raw | 600 / 340 / 415 | 0 | 607–753; 1 s later 254–404 | 548–567 (183–187) | 0.47 / 0.45 s | 78 MiB | 0 / 3 / 30 of 40 |
| 5 [4.2] @175 ms | pcm 48k/16 ×3 | 713 / 368 / 373 | 0 | 733–739; 1 s later 384–394 | 693–891 (188–189) | 0.55 / 0.54 s | 12 MiB | 0 / 3 / 27 of 38 |
| 4 [2.8] @175 ms | pcm 48k/16 ×3 | 777 / 434 / 431 | 0 | 461–817; 1 s later 456–457 | 744–1042 (187–189) | 0.82 / 0.66 s | 10 MiB | 0 / 3 / 25 of 38 |
| 4 [1.4 share] @175 ms + drag-out³ | pcm¹ / flac / flac | 1131 / 533 / 527 | 1 (2.0 s, A)² | 559–654 | 1096–1675 (218–249) | 1.16 / 0.80 s | 12 MiB | 0 / 3 / 23 of 33 |

¹ No estimate yet: the conservative 4 Mbit/s picks the 16-bit pcm preview.
² A concurrent `curl` download of sample B's original took half the link while A played
pcm at the conservative estimate; from the second record on the estimate chose flac and
nothing stalled. The server's separate preview quota keeps the audition from getting
`503`, but bandwidth is shared; switching a running pcm audition to flac on underrun is not
implemented. ³ Run with the build before the "1 s later" seek and the stale-connection retry were added.

These runs predate the per-profile transcode switch: "raw" rows are the old automatic
upgrade at twice the bitrate. The client no longer upgrades on throughput; the bench's
`--transcode off` reproduces the raw rows (what a LAN profile now does by default) and
`--transcode on` (default) the pcm/flac rows.

At 175 ms RTT the first record's TTFA includes opening a connection (~0.5 s); later records
reuse a pooled one (one round trip plus 0.5 s of audio). A click right at the first sound
may still need a new connection (the spare opened when the stream started is not ready
yet); 1 s later it is, and the seek costs a round trip plus the data.

Arrowing at 120 ms starts exactly one stream (the last row); at 300 ms each row starts and
cancels its predecessor: at most 2–3 audio requests at once at the proxy (the closing one,
the new one, and at 175 ms a cancelled one whose close has not reached the proxy yet), no `503`, no non-GET request. The 6-channel and 192 kHz
files never stalled. Waveform JSON is 364 KB–1.07 MB decompressed (zstd on the wire);
revalidation is one round trip. The spare-connection requests (`/health`, about two per
selection) are tiny.

## List thumbnail Waveforms (§9.1–§9.3, client side)

The Waveform column of the result table uses the batch API in server mode; the server
spec and its deviations live in `doc/api/README.md` §9. This section describes what the
client does.

- **Capability.** The column exists only when `/capabilities` has `list_waveforms` with
  `version` 1, `buckets` containing 128 and `encoding` `i16le-minmax-base64`. The client
  reuses the `/capabilities` answer read after each sync (the same read that gates
  removal menus) and makes no request of its own. Until that answer arrives, or when it
  lacks the capability, the column is hidden. There is never a fallback to per-item
  `GET /audio/{id}/waveform`, and the snapshot's `waveform_cache` rows are never read:
  `RemoteProvider::list_waveforms` answers only from the server.
- **Requests.** `POST /api/v1/waveforms` sends up to `max_ids` (at most 200) visible ids,
  plus the ids one screen above and below. Each id carries the `known` etag from the
  client cache. Requests send `Accept-Encoding: zstd` and use the shared keep-alive agent
  and authentication. At most one POST is in flight per client (one worker, plus a mutex
  in `RemoteListWaveforms`). The worker runs in background thread mode. With the client
  setting "hold list-waveform batch requests during first 3 s after audition start"
  (`hold_list_waveform_batches_after_audition`, default off) it sends no new POST until
  3 s after the latest audition stream or preview start (Load/Play/Seek), using the same
  yield hooks as local decoding; held rows are deferred, not dropped. A reply whose `library_id` differs from the
  open snapshot's is discarded (`LibraryChanged`) and nothing is cached. A `503 BUSY`
  defers every lookup by its `Retry-After`, or 1 s when the header is absent. Other
  failures pause lookups for 5 s.
- **Statuses.**
  - `ok`: shown and stored.
  - `not_modified`: shown from the client cache.
  - `pending`: shows an older copy if one exists. The id is asked again after
    `retry_after` (clamped to 1–30 s, 2 s if absent), only while the row is still among
    the visible rows (not the ±1-screen margin), and at most 5 times per id.
  - `missing`: the "no Waveform" placeholder, never retried.
  - `root_offline`: keeps the stale copy, dimmed under the warning tint, or shows an
    offline placeholder. The ids are asked again once a revision poll or a root check
    reports that root `online`.
  - `error`: the "no Waveform" placeholder, with no automatic retry.
  - A status the client does not know is treated as `pending`.
  - A new snapshot, library or source clears all of this state.
- **Client disk cache.**
  - Location: `<data dir>/cache/waveforms/server/<fnv1a64(library_id) as 16 hex>/list/`.
  - Format: client-private shards of 1024 ids each (`<id / 1024>.lws`), holding id, etag
    and peaks, protected by a checksum.
  - Writes: each touched shard is written atomically (temporary file, then rename).
  - Eviction: least recently used shards are evicted past 640 MiB. Shards written by the
    current batch are never evicted.
  - Damaged shards and leftover temporary files are dropped on scan.
  - Memory: one memory cache holds the cells of recently visible rows (4096 entries).
  - Server items carry no `samples_per_pixel`, so the client stores 1.
- **Settings page.** The 服务器音效库 page shows the 「列表缩略波形」 section only when
  the capability is usable.
  - The `pregenerate_list_waveforms` toggle reads `GET /api/v1/settings`. It is disabled
    when `can_change` is false, and changes go through `PATCH /api/v1/settings` with the
    usual library-identity check.
  - Progress comes from `GET /api/v1/waveforms/status`, read with the page and then
    every 5 s, only while the page stays open on that server.
- **波形缓存 (Waveform cache) section** (API §9.5 as implemented at server commit cdea7c9), shown only for the active, open
  server source. All requests go through the server `DataProvider`
  (`waveform_cache_*` methods on `RemoteProvider`; local providers answer
  unsupported) on worker threads.
  - Shows `run_state`, a progress bar (the active job's `done / total`, otherwise
    `cached / total`), `pending`, `failed`, `current_rate` with the effective rate limit,
    the ETA (「估算中」 while `eta_secs` is `null`), the job line (including the
    `skipped_offline` count and the `mode_effective` fallback note) and `last_error`. Status is polled every 2 s only
    while the page stays open; any page, selection or window change stops the poll.
  - With `list_waveform_jobs`: 补齐缺失 / 重试失败 (only when something failed) /
    全部重建 (two inline confirmations; only then `"confirm": "rebuild"` is sent), and
    暂停 / 继续 / 停止 through `/waveforms/jobs/current/*`; the rate field
    (1–1000, empty = server default) patches `list_waveform_rate` when `rate_setting`
    is listed. Only modes listed in `modes` are offered.
  - Without the capability (older servers): only the switch, status and pre-generation
    pause/resume (`/waveforms/pregeneration/*`).
  - `403 MANAGE_FORBIDDEN` (or `can_manage` false) disables the controls until the page
    reloads; `409 BUSY` and `409 JOB_STATE` (`RemoteError::JobState`) show a message and
    refresh the status.

Read-only measurements (2026-10-19, LAN test server, about 0.9 M records, pre-generation
off, release build; ids and server identity omitted). Each page held 200 consecutive ids
that had not been generated:

| Request | Result | Latency |
|---|---|---|
| First POST, cold range | 200 `pending`, `retry_after` 30 | 0.5–2.0 s |
| Repeated POST right after | 200 `pending` | 6 ms (one run: 2.0 s) |
| Retry after 30 s | 200 `ok` | 11 ms |
| Retry every 2 s | 25→18 ids `ok` per poll; all 200 after 7 polls (about 14 s) | 2–6 ms |
| POST with 200 `known` etags | 200 `not_modified` | 5–6 ms |
| Lookup through the provider, warm disk cache | 200 ready | 6–9 ms |

The zstd reply for 200 `ok` items was 88.9 KB, against 154.7 KB uncompressed. The disk
cache for 200 items was 108 KiB.

## Drag-out (files to other programs)

Result rows can be dragged to Explorer, a DAW or any program that accepts dropped files.
Dragging a checked row drags all checked rows; otherwise only the dragged row. The drag is a
native drag (`audiolib_windows::drag_files`): on Windows OLE `DoDragDrop` + `CF_HDROP`, on
macOS AppKit `beginDraggingSession` + `NSURL`/`public.file-url`, copy-only, so the target
always copies and never moves a file.
There are no virtual or delayed-rendered files.

- **Local mode** offers the library files themselves (missing records are skipped).
- **Server mode** first makes sure every selected file is completely present in the
  download cache, then starts the drag with those paths. While downloading, the status bar
  shows `Preparing drag i/n: <name> <percent>` with a **Cancel** button. If the mouse
  button was released before the files were ready, the status reads "N file(s) ready; drag
  again" and the next drag starts at once from the cache.

Download cache (`audiolib_remote::DownloadCache`, `<data dir>/cache/`):

```
cache/<library_id>/<file_id>-<etag token>/<original filename>
```

- `<library_id>` is the same safe directory name as the snapshot cache; `<etag token>` is
  16 hex digits of a hash of the ETag (a changed file gets a new directory, older versions
  of that id are removed).
- The file name comes from the `download=1` response's `Content-Disposition`:
  `filename*=UTF-8''…` (RFC 5987, percent-decoded) is preferred over `filename="…"`; the
  record's file name is the fallback. Names are sanitized for Windows: `<>:"/\|?*` and
  control characters become `_`, trailing dots/spaces are trimmed, reserved device names
  (`CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9`, also with an extension) get a `_` prefix,
  and long names are shortened keeping the extension. Example:
  `filename="audio-7.wav"; filename*=UTF-8''%E9%9B%B7%E5%A3%B0%20%E8%BF%91.wav` → `雷声 近.wav`.
- Reuse: an existing complete entry is checked with `HEAD …/stream?download=1` (same ETag
  as `/stream`); if the ETag still matches it is used as is. That revalidation waits at
  most 2.5 s (`CACHED_REVALIDATE_TIMEOUT`): on a timeout, an offline root or a starting or
  busy server the existing file is used rather than waiting for a disk to wake up. If the
  playback cache already
  holds the complete file (ETag sidecar from the same `/audio/{id}/stream` URL, same length)
  it is copied instead of downloaded. Otherwise `GET /audio/{id}/stream?download=1` writes
  `<name>.part` with `Content-Length` driving the progress; an interrupted download
  resumes with `Range` + `If-Range`; the ETag and length are verified before the rename.
  Offline, the newest cached version of the file is used.
- Size cap 4 GiB with least-recently-used eviction (entry directories' modification time,
  touched on use). Eviction is confined to library folders under `cache/` that carry the
  `.audiolib-downloads` marker and to `<id>-<16 hex>` entry directories inside them; it
  never follows links and never touches other content of `cache/` (for example
  `cache/waveforms`). Files of the current drag are never evicted; stale `.part` files are
  removed after a day.
## Scale (measured)

Synthetic libraries of the server schema (85,000 rows, 1,783 folders, Chinese and English
names, tags and descriptions), served by the real `audiolib-server` on loopback. The
release build ran on the development PC. The table below describes those synthetic
fixtures; production-library counts and capacity are omitted from public documentation.

| Measure | 160 MB snapshot | 645 MB snapshot |
|---|---|---|
| first sync total (CLI) | 4.8 s | 9.2 s (16.9 s before in-place validation) |
| server preparation before the first byte | — | ~7.0 s (transfer 0.3 s on loopback) |
| validation | 2.9 s | 1.4 s (9.2 s with the old temp copy) |
| peak memory during sync (CLI) | — | working set 287 MB (mapped file pages), private 50 MB |
| desktop first sync / library switch | installed 5.0 s | installed 10.2 s, peak working set 561 MB, private 80 MB |
| UI responsiveness during sync (IPC `status`) | median 30 ms, max 39 ms | median 36 ms, max 49 ms |
| peak disk in `remote/` during a switch | — | 774 MB (new 615 MB + old 160 MB until the swap) |
| open snapshot + first stats | 1–80 ms | 38 ms + 65 ms |
| FTS search (common term, two terms, Chinese) | 2–15 ms | 5–8 ms |
| no-hit search / browse page / deep page | < 1 / 3 / 3–12 ms | < 1 / 3 / 3–12 ms |
| stats | 14–21 ms | 8–15 ms |
| full folder tree (1,783) / folder facets | 39 / 8 ms | 30 / 6 ms |
| poll `GET /library` / unchanged sync | 11 / 12.5 ms | 11 / 13.5 ms |
| single `PATCH` (HTTP + apply to the open snapshot) | 3.4 + 37.5 ms | — |

Result lists are paged (200 rows) and the folder tree loads lazily, so the UI never holds
all rows. Sync, polling and streaming run on worker threads. When synchronization is
required it downloads a full validated snapshot; known write replies can advance the
cache directly when their revision matches. The auto-sync gate keeps batched
scans from causing repeated downloads. `cargo run --release -p audiolib-remote --example
scale-probe -- --server URL --data DIR` reproduces the timings (use a temporary `DIR`: the
real `%LOCALAPPDATA%\AudioLibExplorer` is refused; the optional PATCH timing,
`--patch-id ID`, writes to the server and needs `--allow-write`, so test servers only).

Limits: snapshot cancellation closes the scoped connection and polls blocked reads at
most every 100 ms, then drains the watcher and owned preparation/cleanup before returning.
An uncancelled slow body retains the configured 1 h timeout. Search is
token-based FTS: a partial word ("expl") does not match "explosion".

## Desktop usage

The 「音效库管理」 window combines library management and data-source settings.
Open it from *Library → 音效库管理…* (Ctrl+D). **本地设置 / Local settings** selects the
local library; **服务器设置 / Server settings** selects the first saved server or opens
a new server form if none exists. Selecting a settings page does not activate that source.
File now contains only Exit. Add folders from Local settings; Ctrl+Shift+I still opens
the folder scan dialog (also available in Scan). The mode label in the header and 设置…
on server notices open the active connection page.
*扫描历史… / 最近播放…* open the history (server: on 缓存与导出, from the snapshot).

- Left: 本地素材库 and each saved server (● = active, address, last test), Up/Down to
  move the selection, 「＋ 添加服务器」, 「删除此服务器配置…」 (inline confirmation; not for
  the active profile). Leaving the 连接与库信息 page with unsaved edits asks 保存 / 放弃修改 /
  取消 first. Closing the manager or pressing Esc with unsaved server settings asks the
  same question; Save closes only after persistence succeeds, failure keeps the draft,
  and Esc while the question is shown cancels it. 「切换到此数据源」 still switches immediately.
- Right, local: the library management content while local mode is active; in server mode
  an explanation that it needs local mode first.
- Right, server: 「切换到此数据源」 when it is not active, and three pages: **连接与库信息**
  (name, address, token, transcode, revision checks, disabled blank 「WebDAV 设置（功能未开发）」,
  test, 还原 / 保存; a 401 focuses the token field with a hint), **服务器音效库**
  (roots with status, file count, missing count and last scan; management permission;
  active scan progress; latest 20 scans; for servers advertising `root_management`,
  the active source also offers **添加音效库 / Add sound root**, **修改资源路径 / Edit resource path**
  and **删除音效库 / Delete sound root**. The selected row shows its server resource path.
  Browse opens **选择远程文件夹 / Choose remote folder**, with server locations,
  breadcrumbs, parent navigation and an absolute-path field; it never opens a local
  Windows folder picker. Add registers an existing folder; start a scan explicitly to
  index its WAV files. Editing a path requires matching WAV files at the same relative
  paths in the destination and preserves record IDs, Metadata and annotations. Delete
  confirms removal of the root configuration and indexed records/annotations, preserving
  source folders and WAV files. These operations drain requested Metadata writes and
  refresh the snapshot and root mappings; scans and paused scans exclude them. Older
  servers without the capability keep their existing scan/removal controls and require
  a server upgrade for root management. For the active server with management
  permission: 开始扫描… / 暂停 / 继续 / 取消扫描, and 移除所选记录… / 删除目录记录…,
  each confirmed inline — only library records are removed, WAV files on the server
  stay, a rescan re-adds them with new ids) and **缓存与导出** (the cached snapshot of the profile's library; for the active server 立即同步 and the library management content working on the snapshot: history, missing files, check, backup copy, export. Local 「扫描与更新」 buttons stay on 本地设置 only; this page shows a one-line note pointing at 服务器音效库 for scans, and ends the cache options with **试听/性能** for the hold-list-waveform-batches checkbox).

1. *Library → 音效库管理… → 服务器设置*, then 「＋ 添加服务器」 (or select a saved server).
2. Enter the server address (full URL such as `https://audiolib.example.com:8443` or
   `http://192.0.2.10:8790`, or domain + port) and the access token if the server
   requires one (not needed on the LAN; external addresses need a token generated on the
   server's `/admin` page). WebDAV is a disabled placeholder; saving server settings
   preserves existing WebDAV settings and credentials.
3. **Test connection** shows √/×/!/– per step with latency. Steps:
   - **DNS / TCP**: resolved addresses and DNS/TCP latency; DNS failures and refused
     connections are named. On failure the remaining steps are skipped.
   - **TLS handshake / certificate** (https only; for http a note that the connection is
     unencrypted): protocol, cipher suite, certificate subject, issuer, expiry and the
     trust store that verified it, or the classified certificate error.
   - **Server health**, **Token check / library**, **Scan roots**: required. Health shows
     the server's external address when it announces a different one.
   - **Audio stream #id**: `GET /audio/{id}/stream` with `Range: bytes=0-0` on the first
     present (not missing) record of the synced snapshot of the server's library (body not
     read). `206` → √ with the file length and ETag; `200` → √ "server ignored Range";
     `401`, `503` (also `ROOT_OFFLINE`), unreachable and timeouts → × (required; a timeout
     adds the disk wake-up hint). A `404` (that record's file is missing on the
     server) is a warning (!). Without a synced snapshot of that library the step is `–`
     ("not checked: sync to sample a record").
   - **Preview throughput #id** (optional): `GET /audio/{id}/preview?codec=pcm&sr=48000&bits=16`
     with `Range: bytes=0-524287` for the same record; reports time to first byte, Mbit/s
     and the `X-Preview-*` format. Never fails the test.
   - **WebDAV <root>**: optional, because the server stream is the primary audio source.
     A root without a WebDAV URL is `–` "no WebDAV URL configured; playback uses the server
     stream"; a configured URL that fails is a warning (!), never an overall failure.
   The report is successful when every required step passes; the closing line then reads
   "All checks passed." or "Connection OK; N optional check(s) failed (!)". In JSON each
   step carries `ok`, `skipped` and `optional`.
4. **保存** stores the profile; **切换到此数据源** (or 保存并切换) syncs and switches to
   its snapshot. The header shows `Server mode · host · library id · revision · synced time`.
5. **Sync now** downloads a new snapshot when the server revision changed. Between syncs
   the revision is checked in the background (interval and auto-sync on the connection page).
6. Select **本地素材库** and 切换到此数据源 to return to the local working library.

## CLI

```
AudioLibCli remote status [--url URL]  # data source, parsed address, TLS policy, cached snapshot (no network)
AudioLibCli remote test [--url URL]    # per-step connection report (exit code 1 on failure)
AudioLibCli remote sync [--force]
AudioLibCli remote edit <id[,id…]> [--tags T] [--tag-mode replace|append|remove]
                        [--color '#RRGGBB'|''] [--notes N] [--favorite true|false]
AudioLibCli remote download <id[,id…]>
AudioLibCli remote profiles                 # saved servers, active source (no tokens, no network)
AudioLibCli remote roots [--url URL]        # /roots incl. missing_count, scanning, last_scan
AudioLibCli remote capabilities [--url URL] # manage_mode (unknown on servers without /capabilities)
AudioLibCli remote scans [--id N] [--url URL]   # running scan or one session (read-only)
AudioLibCli remote history scans|plays [--limit N] [--offset N] [--url URL]
```

`remote scans` never starts, pauses or cancels a scan.

`remote edit` needs a synced snapshot. One id with `--tag-mode replace` uses `PATCH`,
otherwise the batch endpoint. The reply lists `revision_before`, `revision` and the cache
effect: `{"applied": true, "revision": N}` when the revision rule applied the records, or
`{"applied": false, "sync": {…}}` with the result of the full sync that ran instead (also
after an uncertain error such as `408`). Playback tracking is retired; there is no
`remote plays` command. `remote history plays` returns an empty list without an HTTP request.
`remote download` fills the drag-out download cache and prints `id`, `path` and `bytes`
per file.

`--url` (e.g. `https://audiolib.example.com:8443`) uses that address for this run only; it
is normalised like the dialog input and never saved, and the saved token is still sent.
`remote status` adds `address` (`base_url`, `api_url`, `scheme`, `host`, effective
`port`, `path_prefix`) and `tls` (verification policy, `trust_store`,
`self_signed_accepted: false`). `remote test` adds `server_url` (normalised), `tls` (the
handshake: `protocol`, `cipher_suite`, `server_name`, `subject`, `issuer`, `not_after`,
`chain_length`, `trust_store`) and `public_url` to the report.

`remote test` fails (`REMOTE_TEST_FAILED`, exit code 1) only when a required step fails;
failed optional steps (WebDAV, a sampled record missing on the server) are listed in the
envelope's `warnings` array and the command still succeeds.

The CLI uses the same settings and secrets file as the desktop and never prints secrets.

## Tests

`pwsh -File tools/validate-remote-resume.ps1 -BinaryDirectory <current-binary-directory>`
builds an owned 32-WAV synthetic library, obtains its valid zstd snapshot from an owned
loopback server, and injects truncated/silent response bodies with a second loopback
listener. It checks automatic Range recovery after truncation and after an actual
30-second silent-read timeout, a new CLI process reusing the saved
prefix, preservation of an existing validated cache after an interrupted replacement,
explicit `--restart` beginning at zero, and native desktop WM_CLOSE during a
silent body followed by desktop restart/resume. Requests must retain the strong ETag
and snapshot ID. `-ServerBinary <path>` selects a separately built current server;
`-SkipDesktop` runs only CLI cases and is reported as such. All settings, snapshots,
source WAVs and server state live in a checked owned temporary tree, and normal
environment variables are restored. Failure preserves that fixture for diagnosis.
The report is `.rust-validation/remote-resume/report.json`; it describes synthetic
loopback coverage, not real WAN performance, CLI Ctrl+C or server graceful shutdown.

`cargo test -p audiolib-remote` runs the client against a loopback `TcpListener` test
server (no external network). HTTPS tests use an rcgen test CA (trusted through a
test-only root override; production always uses the OS verifier) and a rustls loopback
server: chain verification and Bearer tokens on API and audio requests, missing/wrong
token messages, untrusted/expired/wrong-name certificates (never bypassed), plain HTTP on
an https URL, refused connections and DNS failures (DNS checks are skipped on networks with
a fake-IP DNS proxy that resolves every name), handshake details and the connection test.
`audiolib-core` tests cover address normalisation and its errors, and the per-profile
transcode defaults and migration (address defaults, `audition_original` folded in,
external siblings on, the dialog's untouched/clicked choice). Also: error mapping, header metadata, BUSY retry, Content-Length
truncation, snapshot validation, URL precedence and mapping, secrets round trip, LRU
eviction, the connection report, the route choice (transcoding off always streams the original
whatever the throughput; on: pcm with `sr`/`bits` even on a fast link, flac below 2.5 Mbit/s and mono below 0.8, `ch=2`, the 415/503
fallbacks), the throughput estimator, and in-memory streaming (partial data read through
`MemoryReader`, window and whole-entry LRU eviction, reuse with `If-Range` and version
replacement, far seek re-request, superseded selections closing their connections (open
sockets counted on the server side), pcm preview headers, FLAC previews decoded from a
hand-encoded FLAC with `t=` seek mapping and silence, a server ignoring `Range`, a file
changed mid-stream, `404`/`401`/`503`/empty files, `503 BUSY` retry and its bound,
cancellation, the per-read stall limit), metadata writes (revision applied vs. full-sync fallback, batch order and
de-duplication, error mapping incl. `413`, `408` → sync, plays without body) and the
download cache (`Content-Disposition` parsing and sanitizing, ETag revalidation with
`HEAD`, reuse of the playback copy, `.part` resume, offline reuse, eviction confined to
`cache/`). `audiolib-db` applies returned records to a snapshot; `audiolib-windows`
checks the `CF_HDROP` data object (paths read back with `DragQueryFileW`, missing files
refused). The desktop server-mode test saves an annotation through a loopback write
server and checks that the snapshot record and revision change without a sync; a UI test
covers the drag gesture threshold and Cancel. `audiolib-audio` tests decode
through a registered in-memory byte source; the desktop server-mode test streams a WAV
from a loopback server through the registered source. Use `AUDIOLIB_DATA_DIR` temporary directories for any
manual test; never point tests at the real library.

### Compact i8 list Waveforms (`i8-minmax-base64`, client side)

- **Gating.** The client adds `"encoding": "i8-minmax-base64"` to `POST /waveforms` only
  when `GET /capabilities` `list_waveforms.encodings` lists it. Otherwise the request and
  the int16 (`i16le-minmax-base64`) path are unchanged.
- **Format.** Up to 128 columns of interleaved signed i8 `min,max` (256 bytes), standard
  base64 of exactly `4 * ceil(2n / 3)` characters; `n = 0` and `n < 128` follow the int16
  rules. `i8 = round(i16 * 127 / 32767)`, half away from zero; a received `-128` is clamped
  to `-127`; a column with `min > max` fails that item.
- **Checks.** The reply must echo `encoding` (and any per-item `encoding`) as requested;
  otherwise the reply is a protocol error: none of its items is cached and every id backs
  off like `pending` (2 s), still showing an older i8 copy. `ok` entity tags must end with
  `-l1i8`, and `not_modified` tags are compared in full. An error body with
  `"sub_code": "UNSUPPORTED_ENCODING"` (doc/api §9.4, `400 VALIDATION`) switches the
  client back to int16 for the rest of the session.
- **Storage.** `waveforms.db` (SQLite, WAL, `synchronous=NORMAL`) next to the library's
  list cache directory, laid out like the server file of doc/api §9.4.1:
  `list_waveforms(file_id INTEGER PRIMARY KEY, etag, src_mtime, src_size, encoding, n,
  blob)` with `blob` exactly 256 bytes (zero padded after `n` columns) plus
  `meta(key, value)`. One UPSERT per row, recording the encoding (server items store
  `src_mtime`/`src_size` 0). Local libraries can derive the same rows (etag/zoom key
  `list128-v4-l1i8`) from their `list128-v4-l1` rows without decoding audio, in a
  `waveforms.db` beside the library file; the library schema is unchanged.
- **Bundle (doc/api §9.4.2).** When `list_waveforms.bundle` is true and the local
  `waveforms.db` is empty, a background thread downloads
  `GET /waveforms/bundle?encoding=i8-minmax-base64`, streaming it to a temporary file
  beside `waveforms.db`. It checks `Content-Length`, the `ETag` `"wfb1-<32 hex>"` against
  the first 16 bytes of the file's SHA-256, and `X-Library-Id`. It then validates the file:
  `integrity_check`, exact tables and columns, no views, triggers or extra indexes, every
  `blob` 256 bytes, the encoding, `0 <= n <= 128` and `meta.library_id`. After that it
  closes the store and renames the file over it. Any failure deletes the temporary file
  and keeps the current store. `503 BUSY` waits for the larger of `Retry-After` and 5 s
  doubling, at most 5 minutes; page lookups continue meanwhile. Lookups then resume
  incrementally, sending the bundle's per-entry etags in `known`. `meta.revision` is
  never used to judge freshness.
- **Display.** Values are expanded to the int16 scale (`round(i8 * 32767 / 127)`) and drawn
  at true amplitude, without normalization.
