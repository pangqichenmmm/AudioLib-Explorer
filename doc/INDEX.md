# AudioLib Explorer Documentation Index

AudioLib Explorer is a Rust + Slint Windows desktop application with SQLite FTS5 search, streaming audio playback and English/Chinese runtime switching. The primary performance target is **P95 latency <500ms for 1M+ records**.

## Current implementation

| Document | Purpose |
|----------|---------|
| [README](../README.md) | Build, launch, user-data layout and release operations |
| [Repository guidance](../AGENTS.md) | Engineering invariants and required checks |
| [Rust Architecture](Rust_Architecture.md) | Owned contracts, data services, migration, playback and platform boundaries |
| [Appearance themes](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/beta/doc/Rust_Themes.md) | Stable default and Studio player presentations, settings and theme extension |
| [Search Tasks](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Search_Tasks.md) | Independent search tabs, personal task favorites and session recovery |
| [Application icon](Rust_App_Icon.md) | Windows high DPI icon, macOS Retina assets and regeneration |
| [WAV Metadata](Rust_Wav_Metadata.md) | Embedded BWF/INFO/iXML/XMP/ID3 extraction, search, rescan upgrades and private-format limits |
| [Library Management](Rust_Library_Management.md) | Multi-library selection, backup/restore, integrity, relinking and import/export |
| [Album Artwork](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Artwork.md) | Local/server covers, custom images, hiding/restoring and thumbnail cache management |
| [Shared Albums and Metadata Storage](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Shared_Albums.md) | Schema 8 ownership, namespaces, original Metadata TEXT and safe migration |
| [Search Upgrade Recovery](Rust_Upgrade_Recovery.md) | Committed upgrade batches, crash recovery, startup progress, WAL and snapshot cleanup |
| [Remote Mode (client)](Rust_Remote_Mode.md) | Server data source: settings/secrets, snapshot sync, WebDAV URL mapping, read-only rules and playback cache |
| [Server API](api/README.md) | `server/audiolib-server` HTTP contract and client behaviour |
| [Spectral Peaks and Audition](Rust_Spectral_Audition.md) | Waveform colors, bounded analysis and playback transforms |
| [Debug Mode and Bug Reports](Rust_Debug_Reports.md) | Session-only diagnostic recording, private-data boundaries, report preview and GitHub handoff |
| [Rust Validation](Rust_Validation.md) | Actual checks, performance measurements and hardware limits |
| [Testing Strategy](Testing_Strategy.md) | Scope selection, stage responsibilities and reuse of passing results |
| 0.4.0 pre-push CR validation | Five reviewed fixes, frozen source hashes, default/ASIO checks and verified fixed local installation |
| 0.4.0 merge validation | Merged source identity, default/ASIO checks, schema 4 server compatibility and fixed local installation |
| CR final measurements | Final-source default/ASIO checks and 90 fresh GUI searches, including mixed legacy/epoch IDs and retained pre-fix failures |
| Query/GUI review measurements | 2026-10-11 synthetic 1M-row samples, cache phases and build/test identity |
| First GUI search measurements | Fresh-process 1M-row GUI and directory statistics, including 20001 database folders with stale ANALYZE |
| [Scan and Search Performance](Rust_Scan_Search_Performance.md) | Bounded Metadata workers, batch writes and measured scan/search latency |
| [Large-library GUI Search](Rust_GUI_Search_Performance.md) | Query cancellation, directory facet reuse, virtualized tree models and GUI benchmark |
| [Release Notes](Rust_Release_Notes.md) | Changes in the current Windows portable release |
| [0.8.7 release preparation](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/beta/doc/Rust_Release_0.8.7.md) | Beta scope, original validation evidence, unresolved gates and delivery plan |
| [Automatic updates](Rust_Auto_Update.md) | Startup GitHub release checks, stable/beta channels, verified downloads and deployment |
| [Linux and Docker delivery](Rust_Linux_Docker_Release.md) | Native Linux server archives, offline amd64/arm64 Docker images, Compose and platform acceptance |
| [Review Fixes](Rust_Review_Fixes.md) | Reviewed defects, corrections and regression coverage |
| [Known issues](Rust_Known_Issues.md) | Reported defects queued for later investigation and acceptance criteria |
| [Portable application guide](../PORTABLE_README.md) | Packaged startup, settings and local-use delivery |
| [Third-party materials](../THIRD_PARTY.md) | Dependency notices and distribution licensing |

JSON and text files named `Rust_*` in this directory record specific verification runs. Their dates and versions matter; historical results do not imply that every measurement was repeated for the latest source revision. Temporary raw logs and databases under `.rust-validation/` are disposable and recreated by the supported validation scripts.

## Code and configuration

The isolated next-version database work is tracked in
[Plan B storage](api/Search_Storage_B.md),
[compatibility and migration](api/Library_Migration_Compatibility.md),
[historical full-library measurements](api/Search_Storage_B_Validation.md), and
[implementation status](api/Implementation_B_Status.md).
[Plan B code review](api/Search_Storage_B_CR.md) records the fixes and their historical validation.
The local candidate `c81eca2` integrates main `c90c0fe` (0.5.1, including keyboard audition) and keeps version `0.6.0-dev.1`;
targeted local validation passed; the optimized UI regression build fix is `c0870a8`. See the [Schema v6 contract](api/Schema_V6_Iteration.md),
[column audit](api/Schema_V6_Column_Audit.md), and [future LAN acceptance](api/Schema_V6_LAN_Acceptance.md).
The exact legacy peer exceptions are 0.4.11 / 0.5.0 / 0.5.1; they do not admit old clients to p1 library writes.
PR #20 remains a draft. This work does not deploy, install or upgrade the working library;
LAN acceptance waits for the user's deployment notification. Full-library evidence retains its original source identity.

| Entry | Responsibility |
|-------|----------------|
| [Desktop entry point](../app/audiolib-desktop/src/main.rs) | Startup, instance lease and Slint event loop |
| [Desktop controller](../app/audiolib-desktop/src/controller.rs) | Background work, current-library generations and UI actions |
| [CLI entry point](../app/audiolib-cli/src/main.rs) | Search, scanning, library management and local playback control |
| [Core contracts](../crates/audiolib-core/src/lib.rs) | DataProvider, owned DTOs and validation |
| [Library contracts](../crates/audiolib-core/src/library.rs) | Management, transfer and history DTOs |
| [SQLite provider](../crates/audiolib-db/src/lib.rs) | FTS5 search, Chinese tokenization and library access |
| [Audio engine](../crates/audiolib-audio/src/engine.rs) | Streaming playback, state and output recovery |
| [Rust settings example](../config/rust_settings.example.json) | Preferences and playback configuration |
| [Audio terminology dictionary](../resources/audio_terms.txt) | Chinese audio terms used for indexing |

The local original database and `config/audiolib_config.yaml` remain read-only import inputs and stay outside Git. The old Python implementation and its launch/test scripts were removed from the working tree; earlier committed versions preserve them.

## Supported verification

Rust unit and integration tests live with the Cargo workspace packages. Select the smallest affected scope in [AGENTS.md](../AGENTS.md#development-checks-by-change-scope) and [Testing Strategy](Testing_Strategy.md): formatting, strict package Clippy and relevant tests for Rust behavior changes, plus direct-consumer compilation for public contracts. Ordinary GitHub-hosted CI checks formatting only; functional validation runs locally. ASIO checks apply to affected audio/device or feature wiring, and full workspace checks apply only to broad or uncertain impact. Release checks also follow changed packages and shipped features.

| Tool | Purpose |
|------|---------|
| [Runtime regression](../tools/validate-runtime.ps1) | Legacy import, scans, search and Metadata preservation |
| [Review regression](../tools/validate-review-fixes.ps1) | Configuration, scan coordination and reviewed data protections |
| [Library management regression](../tools/validate-library-management.ps1) | Backup/restore, transfer, relinking and history |
| [Playback IPC regression](../tools/validate-playback-ipc.ps1) | Muted real playback, transport and play-session history |
| [Access denial regression](../tools/validate-access-denial.ps1) | Owned Windows ACL fixture and safe scan reconciliation |
| [Spectral UI validation](../tools/validate-spectral-ui.ps1) | Waveform and UI snapshots with isolated data |
| [Portable validation](../tools/validate-portable.ps1) | Packaged executable identity, startup and license materials |
| [Rust search benchmark](../crates/audiolib-db/src/bin/search-benchmark.rs) | Actual-service P50/P95/P99 measurements on 1M+ records |
| [Local checks](../tools/check-local.ps1) | Format and selected package Clippy/tests once; -Package scopes checks, -Asio / -NoDefaultFeatures match the shipped configuration |
| [Build cache maintenance](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Build_Cache.md) | Owned Cargo caches only; preview, budget/age, build leases and post-delivery cleanup |
| [Release pipeline](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Release_Pipeline.md) | One native plan per host; compact JSON, private logs and verified stage reuse |
| [Windows release compatibility entry](../tools/ci-release.ps1) | Manual tagged Windows build/package entry, replacing separate build/package calls; runtime report remains `not-run` |
| [Published release verifier](../.github/workflows/release.yml) | Optional manual, read-only release identity, exact asset-set and SHA256 checks |

These scripts are maintained regression tooling. Legacy import acceptance requires its explicit fixture; local portable validation defaults to an isolated empty library and synthetic WAV. Legacy fixture import is explicit (`-LegacyImport`). Validate each actual payload once, then verify installed file hashes and one isolated startup. The hosted release verifier does not build packages or run application acceptance.

## Review and audit records

These records retain the source identity and scope of each review; historical findings are not claims about the current main branch.

| Record | Scope |
|---|---|
| [Client UI review, 2026-10-22](reviews/Client_UI_Review_2026-10-22.md) and [verified fixes](reviews/Client_UI_Fixes_2026-10-22.md) | Original working-copy review plus the subsequent main-branch verification and fixes |
| [Server review, 2026-10-22](reviews/Server_Review_2026-10-22.md) | Static review of the older working copy; its findings require rechecking against the current server |
| [Workspace omissions audit, 2026-10-22](reviews/Workspace_Omissions_Audit_2026-10-22.md) | Comparison of uncommitted work with main, distinguishing already integrated changes from missing process and validation work |

## Retained requirements and historical design

These documents preserve product intent and design history. Python, SQLAlchemy, tkinter, PyQt6 and Alembic examples describe earlier designs; current behavior and commands come from the Rust documents above. Planned DAW parsers, playlist editing and plugins are deferred even where older designs describe them; the delivered server mode is described in [Rust Remote Mode](Rust_Remote_Mode.md).

- [Product Requirements](AudioLib_Explorer_PRD.md) and [Original Requirements](<AudioLib_Explorer Requirement.md>): feature goals, users and acceptance targets.
- [Technical Architecture](Technical_Architecture.md): historical design specification; **Appendix A remains the terminology reference**.
- [Security Guidelines](Security_Guidelines.md) and [Testing Strategy](Testing_Strategy.md): retained security and quality requirements.
- [Phase 1 Implementation Guide](archive/Phase1_Implementation_Guide.md): historical Python implementation plan.
- [Phase 2 Planning](archive/Phase2_Planning_Overview.md) and [API Design](API_Design.md): deferred client/server design.
- [Database Schema Changes](Database_Schema_Changes.md) and [Cross Index Design](archive/Cross_Index_Implementation.md): historical schema/search proposals.
- [Code Optimization Proposal](archive/Code_Optimization_Proposal.md), [UI Redesign Proposal](archive/UI_Redesign_Proposal.md) and [UI Color Scheme Guide](UI_Color_Scheme_Guide.md): historical implementation/UI proposals.

Use **Waveform**, **Metadata**, **FTS5** and **DAW** consistently, and express the primary performance metric as **P95 latency <500ms for 1M+ records**. Measurements must identify dataset size, cache conditions, configuration and remaining limitations.
