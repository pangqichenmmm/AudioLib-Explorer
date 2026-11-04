# Search tasks and saved searches

The desktop supports up to 30 independent search tasks for the active library. Each task owns its query, directory navigation, filters and result-list view. Search-task favorites are personal client state; audio favorites keep their existing library Metadata behavior.

## Using tasks

The document-style task strip sits above the search field. Its active task has a connected content edge and emphasis line, making it distinct from toolbar buttons and the smaller panel selectors. Tabs retain their readable width when their combined width exceeds the window: the strip scrolls horizontally, with left/right controls to move through the overflow. Activating or creating a task reveals its tab automatically. The creation `+` remains fixed beside the scrolling strip so creating a task does not require scrolling to the end. New blank tasks are named `default` in English and `默认搜索` in Chinese; older automatically numbered titles display with the same localized default name, while custom titles remain intact.

Explicit task and favorite renames are stored with their name provenance, so names such as `Search 2` or `default` remain literal across language changes, task switching and restart. Older workspaces without that optional field retain their existing automatic-name behavior.

After a search, the tab shows `Search:` / `搜索:` with compact, unboxed text: pale teal keywords and a different warm text color for the directory name. Neither uses a border or filled badge. The prefix and terms align left and share the tab row's vertical center. The displayed conditions come from the executable search definition, so invalid or unsubmitted editor drafts do not rename the tab's current results. Long conditions elide in one row; hovering the tab reveals the full query, indexed directory path, descendant policy, filters and sorting. A custom task name remains available in the hover summary and rename dialog. Search-task favorite controls are grouped with the filter and details icons at the right of the sound-list header; they always act on the active task.

New tasks start with an empty query, the whole current library and default filters. Closing a task leaves other tasks intact; closing the last task leaves one empty task available. Appearance settings select the maximum number of open tasks from one to 30, with 30 as the default. Previously saved limits, including six, remain personal preferences and can be raised in Appearance settings. Reaching the limit never replaces an existing task. Lowering the limit below the current task count requires closing tasks first.

All tasks use the current data source. Switching between libraries saves the old library's task group and restores the destination library's group. Connections to the same server library share that library's personal group, including LAN and external connection profiles. A different server library has a separate group.

## Independent and shared state

Each task retains its query text, complete search filters, selected directory and descendant policy, directory expansion, sort order, page, selected and checked records, selection anchor and view position. Raw filter drafts are retained too: changing tasks does not submit a partially typed tag or turn invalid duration text into a valid search. The current filter/details page and directory/favorites page are part of the task view.

Column widths, pane widths and visibility, language, theme, interface scale, audio output and playback remain application preferences. Sorting is task-local; the existing table preference supplies the initial sort for a new task.

The audio engine and audition session are shared. Changing or closing a task does not load, start, pause or stop audio. Restoring a task's selected row may restore its details, but it must not pass through an audio-loading selection path. Playback remains subject to the existing explicit transport and selection actions.

Returning to the sound already loaded in the engine also preserves its reload state: an ordinary Pause/Resume continues the same position and play session. A different selected sound or an existing failed explicit load still requires its own successful Load before audition.

After cancelling a previous task's image request, restoration queues the applicable Waveform again without sending Load/Play. A loaded or pending remote playback mapping takes precedence over an unrelated restored inspector row; an empty player can preview the restored selection. Obsolete image publications remain cancelled, and a task with neither source clears the previous image.

Metadata drafts and their ordered write queue are shared by file identity and edit revision. Leaving a task captures requested edits before changing the displayed record. Returning to the same audio in another task shows the current pending draft or confirmed Metadata, rather than a separate editable copy. Closing the application drains requested writes through the existing shutdown barriers.

## Favorites

Favorites has separate audio and search-task categories. Audio favorites continue to filter `AudioFile.favorite` and use the existing local or remote Metadata write path. A search-task favorite saves a named search definition: query, directory scope, descendant policy, complete filters and sorting. It does not freeze the resulting audio records.

Opening a saved search focuses an open task only when its validated search definition still matches the favorite. Otherwise it opens an independent task when capacity is available, preserving any temporary edits in the previous task. Results are queried again against the current library. Changing a task does not change its saved search; the explicit update action replaces the saved definition. Search favorites can be renamed or removed without changing audio Metadata.

Reusing a matching task associates it with the favorite being opened, including when it is already active. Snapshot-assigned directory IDs do not by themselves change the saved search: cross-ID reuse requires the exact stored path spelling. POSIX case and literal backslashes, and Windows directory-entry case, remain significant; Windows drive, UNC host/share and separator spellings are normalized when comparing references to the same ID. Normalization alone never merges two indexed alias rows. Path lookups prefer exact source spellings and refuse ambiguous aliases. Audio and directory lookup pin ID selection and owned-row projection to one read snapshot.

Personal search favorites remain on this computer, independently of server write permission and connectivity. They do not travel in library exports, backups or server snapshots, and do not require an HTTP API or main-library schema change.

## Persistence and recovery

The client stores task order, active task, search definitions, navigation and view state in a versioned `workspace.json` under the configured user data root. The default root is `%LOCALAPPDATA%/AudioLibExplorer/v1`; `AUDIOLIB_DATA_DIR` selects an isolated alternative. Per-library scopes use the existing canonical local-library identity or the server library ID. Tokens, passwords, decoded audio, result rows, Slint models and Waveform images are not persisted in a task workspace.

Schema-8 clients write workspace format 2 and explicitly admit existing format-1
files in memory before the next ordered save. All tasks, favorites and drafts
remain. The user-data directory stays at `v1`. Older clients reject format 2
instead of silently ignoring an album-ID filter; a downgrade must restore a
matching older workspace backup to reuse those conditions. Unknown fields,
unknown filter conditions, foreign scopes and future versions preserve the
original file and open a protected temporary workspace.

All open tasks, including tasks without a favorite, are restored on the next launch. Only the active task queries at startup; other tasks query when activated. Restoration never resumes audio automatically. Persisted file and directory identities include their indexed paths and are resolved against the opened library before reuse. A missing saved directory is reported as an unavailable scope, rather than silently broadening the search to the whole library.

Album filters store the album namespace and Metadata key or folder identity along
with the numeric ID. When an old server adopts native shared albums, all open
and saved filters resolve their semantic owner before querying. A matching
numeric ID alone cannot select a different album. Missing albums remain visibly
unavailable and block execution until reselected or reset; original conditions
are retained. Within one namespace, a supported root relocation keeps the live
folder album ID and updates its stored path.

Workspace writes run off the Slint event loop and retain their original library target. Switching the current source cannot redirect a late save into another library's group. Unknown or damaged workspace formats are preserved and reported; an empty fallback must not overwrite them as an ordinary save.

An unreadable workspace opens a clearly marked, read-only temporary search session. Task and favorite mutations are disabled, and its temporary conditions are not promised to survive restart. Repair the workspace file and reopen the library to resume persistent work. Audio Metadata editing keeps its existing library permissions.

If a task's identity lookup fails, its stored definition and navigation remain intact. The failed view cannot inherit another task's directory, selection or checked records; F5 retries restoration, and other tasks remain selectable. A failed application close resumes any task or workspace load cancelled for that close. Directory expansion references remain preserved while their tree data is still loading.

## Search execution and invalidation

Tasks share the existing bounded result and directory-facet workers. Switching tasks cancels obsolete requests. Every asynchronous result, directory model, detail response and visible-row Waveform publication must match the current task, request and library/source generations before updating the UI.

An inactive task can retain its completed result-page cache in memory. Reuse requires the same provider identity and a matching `DataProvider::search_revision`; a missing or failed revision probe causes a fresh query. Provider replacement invalidates old caches even when revision numbers coincide. Metadata changes, scans, removals and refreshes invalidate dependent task views. Background tasks refresh lazily when selected, avoiding simultaneous whole-library searches for every open task.

Query validation and FTS5 behavior remain the existing core contracts, including the 500-character query limit. The performance target remains **P95 latency <500ms for 1M+ records**. Task behavior tests and layout checks do not establish a new million-record benchmark or physical button-to-audio-output measurement.

## Validation

Regression coverage exercises independent query/filter/directory/list state, capacity and last-task behavior, late asynchronous publications, same-file Metadata drafts across tasks, manual favorite updates and restart restoration. Identity tests reject recycled folder/file IDs, and cache tests observe external Metadata changes. A failed workspace save blocks a library switch and remains failed even after another library's workspace saves successfully.

Local verification on Windows x64 with Rust 1.97.1 MSVC on 2026-10-25 passed formatting, strict Clippy for core/db/remote/desktop/CLI, 105 core tests, five workspace-storage tests, the remote offline-workspace test, 304 desktop tests across the controller and other desktop groups, and 19 CLI tests. After the final cache-revision guard, the 12 task/UI tests and three directory-search regressions passed again. The synthetic package-boundary regression passed with this document included in the portable allowlist. Ignored desktop entries include subprocess helpers exercised by their parent tests and explicitly interactive hardware/UI checks; they are not claimed as separately run acceptance.

The review follow-up on the same date passed 177 controller tests, 19 task/UI regressions, the two core workspace DTO tests, all six workspace-storage cases, and strict Clippy for core/db/desktop. It adds coverage for failed task reads and F5 retries, interrupted close recovery, protected workspace interaction, complete unsubmitted filter validation, cold directory expansion and playback-session continuity. The storage case reopens a real owned library containing zero and negative SQLite identities before saving their references; the complete signed `i64` identity domain is supported.

The tab-presentation follow-up passed formatting, strict desktop Clippy and 24 targeted task/UI tests. These cover legacy default-name localization, executable-condition summaries, quoted keyword display and directory names, active-task toolbar favorite targeting, horizontal scrolling to the inline `+`, and actual delayed hover content with the full query and directory. The scrolling interaction uses supported narrow windows of 1080×700 at 100% and 2160×1400 at 200%; software-rendered hover screenshots were inspected in both languages. After simplifying both keyword and directory labels to unboxed tinted text, all nine UI interaction tests passed again.

The final CR integrates beta commit `7b97a78` and passed strict Clippy for core/db/remote/desktop/CLI/server, 106 core tests, 191 controller tests, 35 task/UI tests, six workspace-storage cases, three folder-identity cases, three existing path-identity unit cases, and ten remote write/lookup cases. The three new identity cases include 512 concurrent transactional ID/path swaps with 1024 paired lookups. The beta integration also passed 19 CLI tests, 318 server tests, five search-fragment cases and two include-tag cases. Isolated child helpers are exercised by their parent tests and are not counted twice. Regressions cover favorite association on reuse, full-capacity reopening after ID drift, case-distinct source paths, explicit names across restart/language changes, and cancelled Waveform successors including remote Loading/Ready/Playing source selection.

On the same Windows x64 host (Ryzen 7 9800X3D, Rust 1.97.1 MSVC), `cargo run --release --locked -p audiolib-db --bin search-benchmark -- --records 1000000 --queries 100` passed on an independent generated temporary library. At code commit `f81401a`, the 100 default mixed result-page queries measured P50 2.11 ms, P95 150.67 ms and P99 194.78 ms, with 65 ranking-cache hits. The initial mixed pass measured P95 308.17 ms. These are DataProvider result-page execution times after fixture generation/indexing, with OS caches retained; directory aggregation, new-provider cold starts, GUI debounce/queue/rendering and physical playback latency were not measured. No other build ran during measurement. The target remains **P95 latency <500ms for 1M+ records**.

Layout regression coverage includes 30 task tabs, horizontal overflow, both desktop languages, the minimum 1080×700 logical window and supported interface scaling. Keyboard navigation must remain available without interpreting task changes as audio transport. Real-device audio and performance acceptance are recorded separately from synthetic UI checks.
