# Codex 交接总索引 — 2026-10-27

English: historical master handoff for Codex after the Grok fleet quota wrap-up on 2026-10-27 (Asia/Shanghai). See the current status below.

本文件是唯一总索引：`doc/HANDOFF-CODEX-20261027.md`。<br>
Debug 播放调查笔记在 `docs/HANDOFF-CODEX-20261027.md`，只作链接，不要把两份文件当成同一份，也不要互相覆盖。

后续接手结果及授权的生产部署见[封面功能说明](Rust_Artwork.md)、[最新验证报告](Rust_Validation.md)、[数据库影响审查](Artwork_Data_Safety_20261027.md)与[播放调查证据](../docs/Playback_Fix_20261027.md)。以下保留交接时状态，不代表这些待办或当时限制仍然适用。发布 Release 或合并 PR 仍需另外授权。

## 1. 分支状态

| 项 | 值 |
| --- | --- |
| 仓库 | [Freedragonlancer/AudioLib-Explorer](https://github.com/Freedragonlancer/AudioLib-Explorer) |
| 分支 | `handoff/codex-20261027` |
| beta 基线 | `1b411215`（本地专辑封面 / schema v7 已在 beta） |
| 本索引的父提交 | `ac30e578`（`docs: Codex handoff 2026-10-27 (Windows client)`） |
| 草稿 PR | [#136](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/136) → `beta`（**保持 draft，禁止合并**） |

v0.8.0 曾经 #134 合入 main（merge `6bf602e3`），随后 **GitHub Release 已删除**（服务器模式预览/播放与曲库封面问题严重）。当前最新 Release 是 **v0.7.8**。附注 tag `v0.8.0` 仍指向 `6bf602e3`，**留给 Paul 决定，不要删 tag**。main 上仍保留这次 promote 历史，管家不做 main 回滚。

## 2. 交接时的约束（历史记录）

- **#97**（design(compat) marker sync）保持原样，不合并、不改、不评论：https://github.com/Freedragonlancer/AudioLib-Explorer/pull/97
- **8790** 生产仍是 v0.7.8 / schema 6。没有 Paul 点头，不升级、不重启、不改部署配置。
- **不要**把 `handoff/codex-20261027` 合并进 `beta`。改动走分支 + PR → `beta`，不直接推 `main`。
- **不要**在没有 Paul 下令时重建 v0.8.0 Release。tag `v0.8.0` 保留到 Paul 决定（新 tag 或复用都要先问）。
- 不删除 Release、tag、分支、issue，也不改可见性，除非 Paul 先列出清单。
- Capability 名锁死为 **`album_artwork`**（rev ≥ 1）。不是 `artwork`。
- 播放修复与封面分成独立关注点，不要塞进同一个实现 PR。

## 3. 三条轨道

| 轨道 | 位置 | 提交 / PR | 笔记 | 状态 |
| --- | --- | --- | --- | --- |
| A. Debug 远程播放 | **本分支** | `3c31c14`；无单独 PR | [`docs/HANDOFF-CODEX-20261027.md`](../docs/HANDOFF-CODEX-20261027.md) | 只调查，无修复 |
| B. Server `album_artwork` | **本分支** | `d807cad`；草稿 [#136](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/136) | [`doc/HANDOFF_SERVER_ALBUM_ARTWORK.md`](HANDOFF_SERVER_ALBUM_ARTWORK.md) | 一期端点已在本分支 |
| C. Client 远程封面 | **另一分支** | `codex/handoff-remote-album-artwork`；草稿 [#135](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/135) **不要合并** | 本分支主笔记 [`doc/handoff/2026-10-27-client-to-codex.md`](handoff/2026-10-27-client-to-codex.md) @ `ac30e578` | 当前不能编译 |

### A. Debug 播放（`3c31c14`）

调查笔记，没有播放代码改动，也没有回归测试。现象：0.8 客户端对生产 8790（v0.7.8 / schema 6）无法正常预览/播放。服务端 stream/preview 相对 0.7.8 视为未改，先看客户端。

优先假设：schema 6 远程快照被误送进 `upgrade_owned_snapshot` / 专辑投影，大库拷贝只读临时库时卡住，看起来像不能播放。其次才查 `playable_path` → `RemoteClient::audio_source` → `StreamSource`（settle、stream vs preview、Range、Load）。代码阅读**尚未证实**主假设；完整 Plan B schema 6 文件可能根本不走拷贝。必须在真实 8790 库上证实或排除。细节只看 Debug 笔记，不要整篇搬进本索引。

### B. Server 封面（`d807cad`）

Capability **`album_artwork`** revision 1。`GET /api/v1/audio/{id}/artwork`（可选 `?side=`）从内嵌封面按需提取。无图 / 缺文件 / 提取失败为 404。像素不写入 SQLite，也不进 snapshot。旧 8790 没有该 capability 时，客户端空白降级；不要为了封面单独升级 8790。专辑侧栏封面是二期（`cover_audio_id` 或 albums artwork）。

### C. Client 封面（不要并进本分支）

- 实现分支：`codex/handoff-remote-album-artwork`
- 草稿 PR（**不要合并**）：https://github.com/Freedragonlancer/AudioLib-Explorer/pull/135
- 本分支主笔记：`doc/handoff/2026-10-27-client-to-codex.md`（`ac30e578`）
- 实现细节笔记在客户端分支，不在本分支：`doc/handoff/2026-10-27-remote-album-artwork-client.md`
- 已做：capability `album_artwork`；`GET /api/v1/audio/{id}/artwork`（`?side=`）；404/405 → 空白；远程选中入队封面；缓存键 = `library_id` + `audio_id` + revision + ETag；本地路径不变
- 未完成：`remote_artwork_library` / `remote_artwork_client` 仍缺失，**当前不能编译**；fmt / clippy / tests 未跑；侧栏是二期；播放回归分开；#97 与 8790 未动

本地封面已在 beta。远程在未接线前仍会于 `remote_read_only` 处提前返回（`queue_artwork` 与 `publish_albums`）。

## 4. Codex 下一步

1. 在真实 8790 库上证实或排除 Debug 的 schema 6 停顿（`upgrade_owned_snapshot` / 投影）。不要升级 8790。播放修复单独进 beta。
2. 补齐客户端 `remote_artwork_library` / `remote_artwork_client`，让 #135 能对着本分支的 `album_artwork` 端点编译。封面与播放保持分开。
3. 本地 / Mac 验证：对 8790 schema 6 空白降级，以及可选的、带 capability 的新服务端。
4. 只有 Paul 点头之后才 promote 并重新发版。现有 tag `v0.8.0` 是新 tag 还是复用，先问 Paul。

## 5. 路径

- 总索引（本文件）：`doc/HANDOFF-CODEX-20261027.md`
- Debug 播放笔记（不要覆盖）：`docs/HANDOFF-CODEX-20261027.md`
- Server 封面笔记：`doc/HANDOFF_SERVER_ALBUM_ARTWORK.md`
- Client 主笔记（本分支）：`doc/handoff/2026-10-27-client-to-codex.md`
