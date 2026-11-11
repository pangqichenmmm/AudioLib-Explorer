# 服务端设计稿：搜索与 Metadata v2（schema 8 → 9）

> 状态：设计稿（仅文档；基准原型在未提交 worktree）。验收口径：每条查询 5 轮、各轮 p95 中位数 ≤ s8 × 1.2。基线：`origin/feat/search-metadata-v2` = beta `4ff16a5`（v0.8.7 = `b5a98ff`）。
> 关联：#204 路径搜索、#205 Vorbis/FLAC/OGG、#206 厂商 CSV/XLS(X)、#207 跟踪；客户端设计 PR #209（`doc/DESIGN-SEARCH-METADATA-V2.md` @ f594cc1）；测试计划 PR #208（附录 A @ b16c742）。
> 实测环境：服务器测试机（macOS arm64），库副本 `<bench-dir>/bench.db`（来源 `<prod-backup>.db`，909,342 行，schema 8，10.64 GB）。
> 所有 <prod-port> 上的动作都要 Paul 批准，本稿不动生产。

## 0. 现状（代码位置）

| 项 | 位置 | 说明 |
|---|---|---|
| schema 常量 | `crates/audiolib-db/src/schema.rs:17-21` | `SCHEMA_VERSION=8`，`MIN_SNAPSHOT_SCHEMA_VERSION=3` |
| 迁移入口 | `schema.rs:911 migrate` / `:915 migrate_with_progress`；`:923` 高于已知版本拒绝 | 按 `user_version` 分派 |
| 可续跑升级 + 回滚备份 | `schema/search_upgrade.rs:73 migrate_resumable`；`:89-115` 先 `rollback_path` 再 `snapshot_to_file_cancellable` 做 `audiolib-rollback-*.db`，校验 integrity/FK；`:134` 写入 library_meta | 迁移前必备份 |
| 当前（v8）FTS 定义 | `crates/audiolib-db/src/search_storage.rs:28-34 CREATE_FTS` | **contentless**：`content='',contentless_delete=1,detail=full,columnsize=1`，12 列（6 原文 + 6 `_tokenized`），`unicode61 remove_diacritics 2` |
| 旧（≤v5）FTS 定义 | `schema.rs:1307 FTS_SCHEMA` | external content（`content='audio_files'`），只用于老库升级 |
| 投影/索引批 | `search_storage.rs:37-48 projection()`、`:110-135 index_batch()`（5000 行一批） | 写入时用 `audiolib_tokenize` 生成字母二元组 `gXXXXXXYYYYYY` |
| 搜索重建 | `schema/current.rs:243 rebuild_search`（整库**一个事务**，DROP + 重建 + 触发器）；`schema.rs:997` 旧版 | CLI：`app/audiolib-cli/src/main.rs:373 RebuildSearch` / `:873 rebuild_search()`；**没有** admin HTTP 端点 |
| 查询编译 | `crates/audiolib-db/src/search_text.rs:9-10 RAW/DERIVED_COLUMNS`、`:203 compile_query` | 字母词 =（原文列短语 OR 派生列二元组短语）；**汉字/数字词无列过滤** |
| 候选查询 | `search_candidates.rs:37` `SELECT rowid FROM audio_search WHERE audio_search MATCH ?` | 加列会直接影响这条 |
| 能力/兼容 | `server/audiolib-server/src/routes.rs:233-239,509-545 GET /capabilities`（含 `compatibility_manifest()`）；`crates/audiolib-core/src/compatibility.rs:9 CATALOG_REVISION`、`:11 TARGET_SCHEMA=8`、`:15 PLAN_B_FORMAT="ale-s8-cld1-t3-m2-p1-a1"`、`:17 …-z1`、`:40-48,514-516` 头名、`:442 features {name:rev}`、`:626 compatibility_manifest()`、`:149 snapshot_format_id()` | 格式 ID 只有完全匹配特征表才生成 |
| 准入 | `server/audiolib-server/src/compatibility.rs:31 admit()`：先 peer 标记，再 `x-audiolib-accept-snapshot-formats` 精确匹配、`snapshot-schema-max`/`search-storage-max`/`tokenization-max`；失败 409 `SNAPSHOT_FORMAT_UNSUPPORTED`（:13）；`:63 rejection()` 版本类 409 | |
| 库身份 | `routes.rs:342` 写操作 409 `LIBRARY_ID_REQUIRED`（需 `X-AudioLib-Library-Id`）；`:2027` artwork 同理 | |
| 健康/升级进度 | `routes.rs:840-858 /health`（`upgrade: service.upgrade_progress()`、`schema_version`、`compatibility_catalog_revision`）；`service.rs:91-100,205-210` | 升级期间业务 503 + Retry-After |
| snapshot | `routes.rs:216 /library/snapshot`、`:2250`；`state.rs:270-340` plain + 首次请求时生成 zstd 副本（level 见 `state.rs:53`）；`state.rs:322` 生成前 `ensure_free_space(plain/8)`；`state.rs:2002-2041 backup_library_cancellable`、`:2076-2087` 从备份检测格式 | 约 959 MB（zstd）、TTFB 约 44 s（首个 zstd 生成） |
| 更新检查 | `server/audiolib-server/src/update.rs:110 start / :153 check`；`admin.rs:60-62 /admin/api/update{,/check,/install}` | 渠道见 `doc/Rust_Auto_Update.md` |
| 扫描器 | `crates/audiolib-db/src/scanner.rs:633` 只收 `.wav` | #205 要扩 |

## 1. 能力和版本协商

### 1.1 新格式 ID 与特征
- `TARGET_SCHEMA 8→9`；新 `PLAN_B_V9_FORMAT = "ale-s9-cld1-t3-m2-p1-a1-sd1"`（`sd1` = 目录段路径索引 rev1，取代原逐文件 `sp1`）。
- **t3 决定（已定）**：NFC + 全角→半角规范化**只作用于目录段索引 `audio_dir_search` 及其查询词**；主表 `audio_search` 不规范化、不重建、一字节不变（`t3` 段含义不变，仍指主表分词版本）。实测副本上 5,985 行路径因此变化。是否同时出 `-z1` 压缩变体沿用 v8 规则。
- `source_search_revision 2 → 3`（Vorbis 组进入 `audiolib_source_text`），`snapshot_format_id()` 增加 `(ContentlessDelete, 9)` 分支。
- **两个修订号的语义**：
  - `source_search_rev`：主表 `audio_search` 的**源文本口径**（哪些字段拼进搜索文本）。rev3 = rev2 + Vorbis 组 + vendor_metadata。它变了表示主表 FTS 需按新口径重算受影响行；8→9 迁移时现有行都是 wav 且 vendor 表为空，新口径产出的文本与 rev2 逐字节相同，所以**迁移不重写主表**（MG-10 哈希不变），之后扫描到的 FLAC/OGG 行、厂商导入行才按 rev3 写入。客户端用它判断本地缓存的主表能否复用。
  - `search_path_rev`：目录段索引的**分词/规范化/布局**版本（1 = t3 规范化、只存本段、根名不入）。它变了只需重建 `audio_dir_search`/`audio_dir_runs`（约 1 s），主表不动；游标里带它，不符返回 409 `CURSOR_STALE`。
- snapshot 新增 3 个 features（进入 `FEATURES` 与格式 ID）：
  - `search_path: 1` —— 目录段路径索引存在（§3：`audio_dir_search` + `audio_dir_runs`）。
  - `vorbis_tags: 1` —— `metadata_json.wav_metadata.vorbis` 组及非 wav 文件可能出现。
  - `vendor_metadata: 1` —— `vendor_metadata` 表/列存在并参与搜索文本。
- `/capabilities` 新字段（只增不改，老客户端忽略）：
```json
{
  "compatibility": { "features": { "search_path": 1, "vorbis_tags": 1, "vendor_metadata": 1, "...": 0 },
                     "snapshot_read_formats": ["...", "ale-s8-cld1-t3-m2-p1-a1", "ale-s9-cld1-t3-m2-p1-a1-sd1"],
                     "database_write_formats": ["ale-s9-cld1-t3-m2-p1-a1-sd1"] },
  "library_schema": { "current": 9, "source_search_rev": 3, "snapshot_formats_served": ["ale-s9-…-sd1", "ale-s8-cld1-t3-m2-p1-a1"] },
  "schema_migration": { "state": "deferred|idle|running|paused|failed|done", "endpoint": "/admin/api/migration" },
  "vendor_import": { "revision": 1, "max_file_bytes": 52428800, "max_rows": 200000, "max_row_text_bytes": 262144 },
  "client_upgrade": { "level": "recommended", "reason": "snapshot_format_downgraded",
                      "missing_features": ["search_path", "vendor_metadata", "vorbis_tags"],
                      "min_client_version": "0.9.0", "served_snapshot_format": "ale-s8-cld1-t3-m2-p1-a1" }
}
```
- `CATALOG_REVISION 1→2`。服务端写格式改为 v9。

### 1.1.1 升级提示与 deferred 状态（提案）
- `/capabilities.client_upgrade`（只在需要提示时出现，没有就是不需要）：
  - `level`：`none` | `recommended`（能用，缺功能）| `required`（本客户端将被 409 拒绝）。
  - `reason`：`snapshot_format_downgraded`（发的是 s8 兼容快照）| `snapshot_format_unsupported` | `feature_unavailable`。
  - `missing_features`：客户端拿不到的 feature 名数组；`min_client_version`：能用全部功能的最低客户端版本；`served_snapshot_format`：本次会发的快照格式。
- 响应头（任意 `/api/v1/*` 响应都可带，客户端只解析不依赖）：
  - `X-AudioLib-Upgrade-Available: recommended; min=0.9.0; features="search_path,vendor_metadata,vorbis_tags"`（取值同 `client_upgrade.level`）。
  - `X-AudioLib-Schema-Migration: deferred; target=9`（服务器已是新二进制、库仍是 s8、等管理员触发；`AUDIOLIB_DEFER_SCHEMA9=1` 时出现）；运行中为 `running; target=9; done=12000; total=20248`；`paused; error=disk_full`；`failed; error=read_only`。
- 示例（deferred 期间旧客户端）：`/capabilities` 返回 `"schema_migration":{"state":"deferred","endpoint":"/admin/api/migration"}`，不返回 `client_upgrade`（还没有新功能可提示）；迁移完成后同一客户端收到 `"client_upgrade":{"level":"recommended","reason":"snapshot_format_downgraded",…}` 和响应头 `X-AudioLib-Upgrade-Available: recommended; min=0.9.0; features="search_path,vendor_metadata,vorbis_tags"`。

### 1.2 降级矩阵
| 客户端 \ 服务端 | 旧服务端（s8） | 新服务端（s9） |
|---|---|---|
| 旧客户端（只认 s8） | 现状 | 若服务端仍保留 s8 兼容快照（§4.1）→ 发 s8，功能缺路径/厂商字段；否则 409 `SNAPSHOT_FORMAT_UNSUPPORTED`，消息提示升级客户端 |
| 新客户端（认 s8+s9） | 客户端看 `/capabilities` 无 `search_path` → 隐藏“路径/厂商”范围开关，照常读 s8 | 正常 |
- 迁移进行中，`/health.upgrade` 带阶段与进度。切换阶段写操作返回 503 `migration_switching`，附 `Retry-After: 30`。迁移是后台批处理时（§2），**旧 s8 快照继续可读**，直到原子切换。

## 2. 迁移流程（8 → 9）

报告 phases 键固定为 `backup / path_build / optimize / switch`（目录表构建计入 `path_build`）。

1. **预检**：
   - 只读（库文件或所在目录不可写）→ `status=failed, error_code=read_only, stage=preflight`，**库不动**（不建表、不建备份），退出码 4。
   - 可用空间 < 库大小 ×1.3 → `status=failed, error_code=disk_full, stage=preflight`，库不动，退出码 3。
2. **backup**：SQLite 在线备份 API（`sqlite3_backup`，每步 65536 页）写 `<db>.audiolib-rollback-mig9.db` + `quick_check`；**永不自动删除**。实测 API 159.4 s vs `VACUUM INTO` 231.2 s，选 API。
3. **path_build**（可续跑）：建 `audio_dir_search`、`audio_dir_runs`、`vendor_metadata`、`v9_checkpoint(phase,last_id,done)`；按 `folders.id` 每批 5000 目录一事务写段索引（delete+insert 幂等，checkpoint 同事务）；再一条语句重建 `audio_dir_runs`。进度 = `done/total`（目录数）。
   - 运行中 `SQLITE_FULL` → 本批回滚，`status=paused, error_code=disk_full`，**checkpoint 保留**，腾出空间后续跑；退出码 5。
4. **optimize**：`INSERT INTO audio_dir_search(audio_dir_search) VALUES('optimize')`。
5. **switch**（单事务，毫秒级）：写 `snapshot_format`、`search_path_rev=1`、`source_search_rev=3`、`user_version=9`，删 checkpoint。`AUDIOLIB_MIGRATION_HOLD_SWITCH_MS` 测试钩子在 switch 前持写锁 N ms（已验证：持锁期间读者照常读 v8、写者 `SQLITE_BUSY`，放锁后 v9）。
6. **新快照**：生成 s9 plain/zstd；s8 兼容快照按 §4.1 保留。
7. **回滚**：switch 前任何失败 → 库仍是 s8（新表可 DROP，无需还原）；switch 后 → 停服用 rollback 备份还原（需 Paul 批准）。

报告示例（`migrate9` 原型实际输出）：
```json
{"status":"done","error_code":null,"phases":{"backup":…,"path_build":…,"optimize":…,"switch":…},"progress":{"done":20248,"total":20248},"invariants":"… => OK","total_s":…}
```

管理接口（局域网限制 **且** `manage` 权限）：
- `GET /admin/api/migration` → `{state, phase, progress:{done,total}, error_code, phases, rollback_backup, started_at}`
- `POST /admin/api/migration/start|pause|resume|cancel`；`POST /admin/api/search/rebuild`。
- `/health.upgrade` 镜像 `state/phase/done/total`。

## 3. 路径搜索：目录段索引（方案 D，取代逐文件路径表）

### 3.1 结构
- `audio_dir_search`：FTS5 contentless（`content='',contentless_delete=1,detail=full,columnsize=1`，`unicode61 remove_diacritics 2`），列 `path_tokens`、`path_tokens_tokenized`，**rowid = `folders.id` = `audio_files.folder_id`（dir_id）**，每个目录一行，只存**本段目录名** `t3_norm(folder_name)`（NFC + 全角→半角）及其 `audiolib_tokenize` 二元组。**库根/挂载前缀不入索引**：路径词只命中根名时路径结果为空。
- `audio_dir_runs(dir_id, lo, hi, n)` WITHOUT ROWID，PK(dir_id, lo)：同一目录下**连续文件 id 段**。生产副本 909,342 文件 = 36,343 段，全部致密（`n = hi-lo+1`）。
- 一个目录“路径命中”某词 ⇔ 它自己或任一祖先段命中（祖先展开在内存里做）。因此改名只重写 1 行段索引，移动目录不重写段索引。
- 进程内 `RunIdx`（按 lo 排序的段、by_dir、父子表、总文件数），约 1 MB，启动时加载，`search_path_rev` 或目录表变更后重载。
- 建表耗时 0.8–1.3 s（bench 副本，含 optimize）。旧的逐文件 `audio_path_search` 不再需要。

### 3.2 查询与排序（all/path/filename；main 保留 s8 行为）
- 每个查询词 `w`：主表命中 = s8 编译的 `compile(w)`；路径命中目录集 = `audio_dir_search MATCH t3(w)` 的目录 + 全部子孙目录。**路径词命中文件数 > 总文件数一半时视为过宽，丢弃该词的路径集（只走主表）**（实测：根名不入索引后 `<root-name>` 路径命中 0；顶层目录 `1_Boom` 8.9%；5 万行不作硬阈值，而是“≤5 万时把候选 runs 限制在子树内”的执行优化）。
- `all` 多词 AND（跨表）：文件命中 ⇔ 每个词都在主表或其路径集中命中（目录匹配掩码做 AND）。例：door 在目录、slam 在文件名可命中，结果按 id 去重。`path` 严格要求每个词均在目录/祖先目录命中；主字段不补足词。`filename` 仅限定文件名及其分词列；`main` 仍覆盖原有主字段，UI 名称为“主字段”。
- 分层排序（keyset 游标 = `(tier_score, id)` 字典序）：
  - 第 0 层「主字段完整命中」：整串在主表命中，分数 = 主表 bm25，顺序与 s8 **逐条一致**（直接执行 s8 的 `ORDER BY bm25,id LIMIT`）。
  - 第 1 层「混合」：部分词在主表、部分在路径，同层按文件 rowid 升序。
  - 第 2 层「纯路径」：全部词只在路径命中，同层按文件 rowid 升序（#208 PS-23/PG-17/VN-13）。
- 执行：主表 TOP-k、目录集和结果 Metadata 共用固定的读事务。混合层按 rowid 升序归并各词主表命中流，每流每批最多 256 行；完整主表命中的排除也使用有界流，续批推进 rowid 下界，不逐文件重启 MATCH、不物化整张命中表。纯路径层按 `dir_runs` 顺序每批最多 256 行展开，只取当前页所需结果及前瞻。
- 目录成员按实际 `folder_id` 核对；不能把 run 范围内的 `folder_id=NULL` 文件认作该目录成员。未分配目录的文件仍计入库文件总数，并可由主字段命中。
- 取消：查询继续使用提供者的取消令牌和 SQLite 中断机制，覆盖同一读事务中的各阶段。
- 带筛选或非相关性排序的 `all/path` 使用参数化 SQL，将逐词 FTS5 目录匹配与闭包成员条件交给 SQLite；不在应用内物化全部文件 ID。目录、专辑、收藏及 Metadata 条件与普通搜索共用。相关性排序仍按以上三层；显式排序作用于整个命中集合，游标同时绑定请求和库修订号。folder/album facets 使用同一集合与读事务。

### 3.3 HTTP 契约
- 请求 DTO：`scope=main|all|path|filename`（默认 `main` = s8 行为）、不透明 `cursor`、`limit`；`offset/page` 保留兼容，**同时传以 cursor 为准**。目录相关性游标与筛选/排序游标使用不同内部编码，调用方不得自行构造。
- 响应使用 `SearchPage`：`results`、`returned_count`、`total_count`、`total_count_exact` 和 `next_cursor`（末页省略）。优化的非空目录分页只观察当前页、一条前瞻及实际跳过的前缀，不为总数遍历全部命中。无 cursor 时 `total_count` 是已观察前缀的数量，有下一页时只是下界；无下一页时，第一页或含结果的页面才确认精确总数。无 cursor 的非零 offset 空页返回 0，未经短路证明全查询为空时 `total_count_exact=false`，不能凭 offset 制造计数。有 cursor 时只计该 cursor 之后当前页和前瞻记录，有下一页时 `total_count_exact=false`，末页为 `true`；末页的标记只确认该游标范围已穷尽，局部计数仍不是全查询总数。短路证明全查询为空时返回精确的 0。主表、文件名、空查询浏览、不可用时的 `all` 主表回退以及 SQL 筛选/排序页保持精确总数。详见 [API 分页契约](api/README.md#5-分页)。
- 目录分页以 `next_cursor` 和已走过的位置导航；内部 `has_more` 不增加到响应中。`total_count_exact=false` 时界面显示“至少/≥”，不能把 cursor 局部计数或下界当最终总数/页数。
- 游标绑定请求条件、provider 会话和数据库变更修订；改查询、范围、筛选、排序、库内容或重开库后必须拒绝旧游标并从第一页重取。DataProvider 的 `CURSOR_STALE` 在现有 HTTP 搜索中映射为409，其他无效输入映射为400 `VALIDATION`。`search_path_rev` 仍描述目录格式版本，不能替代内容修订。
- 现状：服务端已有 `GET /api/v1/search` 和 `POST /api/v1/search`，使用上述四种 scope 和 DataProvider 契约；`POST /search/folders` 尚未实现。RemoteProvider 日常查询仍在校验后的本地只读快照上执行，与本地库共用查询实现，不通过在线搜索路由。迁移工具的搜索动作另投影为 `ids`、`next_cursor`（末页为 `null`），不返回 `SearchPage.total_count`。

### 3.4 实测（bench 副本 909,342 行，schema 8 基线 vs 目录段方案）
口径（Paul 定）：每条查询 5 轮，每轮 3 次预热 + 60 次，取各轮 p95 的中位数，**≤ s8 × 1.2 即过**。

#### 3.4.1 热（ms）：各轮 p95 与中位数
| 查询 | s8 各轮 p95 | s9 各轮 p95 | s8 中位 | s9 中位 | 比值 | 门槛 |
|---|---|---|---|---|---|---|
| slam Foley | [51.52,50.46,50.89,50.96,50.86] | [50.91,50.48,51.24,50.67,53.16] | 50.89 | 50.91 | 1.000 | 过 |
| Doors slam | [43.67,43.68,43.76,43.69,44.31] | [42.33,42.07,42.02,42.07,43.11] | 43.69 | 42.07 | 0.963 | 过 |
| door Construction | [204.34,203.32,203.66,203.99,203.80] | [204.60,204.56,204.50,203.89,205.98] | 203.80 | 204.56 | 1.004 | 过 |
| explosion 1_Boom | [12.72,12.65,12.42,12.21,12.43] | [13.75,14.87,13.75,13.66,13.91] | 12.43 | 13.75 | 1.106 | 过 |
| whoosh Designed | [121.21,121.85,122.06,122.49,122.51] | [121.72,121.52,121.94,121.88,122.77] | 122.06 | 121.88 | 0.999 | 过 |
| <root-name> explosion | [5.30,5.16,5.20,5.05,5.18] | [4.82,4.80,4.78,4.75,4.77] | 5.18 | 4.78 | 0.922 | 过 |
| 爆炸 | [0.05,0.05,0.05,0.05,0.05] | [0.08,0.07,0.07,0.07,0.07] | 0.05 | 0.07 | 1.585 | **不过** |
| 脚步 | [0.05,0.05,0.05,0.05,0.05] | [0.08,0.08,0.08,0.09,0.09] | 0.05 | 0.08 | 1.810 | **不过** |
| explosion | [44.17,42.73,43.51,44.56,43.68] | [43.78,42.97,43.21,43.49,43.83] | 43.68 | 43.49 | 0.996 | 过 |
| whoosh | [56.36,55.66,55.54,55.93,55.66] | [57.63,56.23,55.82,56.00,56.16] | 55.66 | 56.16 | 1.009 | 过 |
| expl | [16.95,16.92,17.06,17.07,17.08] | [17.02,17.10,17.00,17.12,17.13] | 17.06 | 17.10 | 1.002 | 过 |
| door slam | [39.18,39.52,39.30,39.47,39.62] | [39.75,40.00,40.42,42.13,40.03] | 39.47 | 40.03 | 1.014 | 过 |
| Sonomar | [206.25,199.33,219.96,200.21,199.95] | [209.18,220.07,199.61,199.34,200.11] | 200.21 | 200.11 | 0.999 | 过 |
| <root-name> | [4.44,4.53,4.54,4.46,4.50] | [4.55,4.63,4.51,4.59,4.62] | 4.50 | 4.59 | 1.020 | 过 |
| 1_Boom | [19.68,21.49,19.65,19.67,19.65] | [19.79,20.26,19.80,20.24,19.94] | 19.67 | 19.94 | 1.014 | 过 |
| Construction Kit | [247.19,247.03,244.61,246.87,244.56] | [246.26,245.21,245.58,245.95,246.17] | 246.87 | 245.95 | 0.996 | 过 |
| Crowds Concert | [114.26,115.52,115.87,114.07,114.22] | [114.66,114.72,114.55,114.78,114.50] | 114.26 | 114.66 | 1.003 | 过 |
| CREAHmn | [5.86,5.89,5.84,5.92,5.82] | [5.94,5.85,5.95,5.89,6.47] | 5.86 | 5.94 | 1.013 | 过 |
| ROCKMvmt | [6.21,6.09,6.20,6.53,6.21] | [6.13,6.18,6.43,6.25,6.16] | 6.21 | 6.18 | 0.996 | 过 |
| EXPLMisc | [13.69,11.75,11.84,12.29,11.61] | [11.76,11.68,11.89,11.74,11.80] | 11.84 | 11.76 | 0.993 | 过 |

**18/20 闸门通过；`爆炸`/`脚步` 按约定记过（绝对值 <0.1 ms）。** 此前未过的 `<root-name> explosion` 已修好：
- 根因：结果为空，但为证混合层空，曾与主查询并行扫 `<root-name>` 主表命中（约 5.3 ms），把主查询从 ~5.0 拖到 ~5.9（当时 ×1.225）。
- 修法：目录匹配后，对**路径集为空**的词先做主表 `LIMIT 1`；若该词主表也无命中，则所有分层都为空，直接返回，不再跑整串主查询、也不并行全量 nopath。该词主表命中为 0 时整查询短路（本例 `<root-name>` 主表 0 行，约 4.3 ms 证空）。
- 修好后 5 轮 p95 中位：s8 5.18 → s9 4.78（×0.922）。
- `爆炸`/`脚步`：仍 ×1.5–1.8，绝对值 0.05→0.07/0.09 ms，按过关记。

PG-16 相关单列（各轮 p95 中位数，ms）：`<root-name>` 4.50→4.59（×1.02，修前 ×140）；`1_Boom` 19.67→19.94（×1.01，修前 ×2.6）；`explosion 1_Boom` 12.43→13.75（×1.11，其中按目录取文件 `dir_files_page` p50 1.18 ms）；跨表 `slam Foley` 50.89→50.91（混合层 202 条）、`door Construction` 203.80→204.56（混合 140）。分阶段 p50（warmq，60 次）：`<root-name> explosion` dir_match 0.18 / nopath_main 4.31（LIMIT1 证空短路，无 main_bm25）；`explosion 1_Boom` dir_match 0.23 / main_bm25 12.25 / dir_files_page 1.18；`脚步` dir_match 0.02 / dir_files_page 0.02。

#### 3.4.2 冷态
OS 冷缓存需要 `sudo purge`，由 Paul 在终端运行 `<bench-dir>/cold.sh`（每轮 purge 后单次冷样本，5 轮×10 次，输出各轮 p95 + 中位数 + ×1.2 判定）。SQLite 冷（OS 热）单次探针：`<root-name>` 6.06 vs 5.99、`1_Boom` 21.59 vs 21.76、`explosion 1_Boom` 16.2 vs 14.7、`door slam` 40.4 vs 41.4 ms。

#### 3.4.3 正确性 / 翻页 / 取消
- 20 条查询：第 0 层前缀与 s8 首页逐条一致（S8ORDER=true）；与逐文件路径表的 oracle 结果集比较 missing=0 / extra=0；keyset 翻 5 页 dup=0、顺序不一致=0。
- 取消：`<root-name>`、`door slam`、`explosion 1_Boom` 各 20 次，20/20 中断，取消到返回 p50 0.01–0.02 ms。

### 3.5 目录表维护（扫描/改名/移动/删除）
在扫描写库的同一事务里，按本次 diff 得到 `dirs_changed`（新建/改名/移动的目录及其全部子孙、删除的目录）与 `dirs_files_changed`（文件增删、跨目录移动涉及的目录）：
1. 段索引：对 `dirs_changed` 中每个 id `DELETE FROM audio_dir_search WHERE rowid=?` 再按 `folders` 现值插入（目录已删则插入为空操作）。改名只重写该目录 1 行（子孙靠内存祖先展开，自动生效）；移动目录时段名不变，段索引不重写，祖先关系随 `folders` 现值重载。
2. 孤儿清扫（PS-21）：`DELETE FROM audio_dir_runs WHERE dir_id NOT IN (SELECT id FROM folders)`。
3. runs 重算：对 `dirs_files_changed` 每个目录删掉旧段，按全局 id 相邻关系在该目录 id 跨度内重算（段不会跨别的目录的文件）。
4. 同事务提交后 `search_path_rev` 不变，但递增 `dir_index_gen`，读侧据此重载 RunIdx；旧游标因 id 不变仍有效。
5. 不变量（PS-21/PS-22，迁移、全量扫描、增量扫描后都校验）：`count(audio_dir_search) = count(folders)`、孤儿 runs = 0、runs 覆盖文件数 = `sum(n)` = `count(audio_files)`、没有 run 含别的目录的文件。
实测（maint-test，同一事务后回滚）：删 1 个叶目录（243 文件）+ 跨目录移动 1 文件 + 新增 3 文件 + 改名 1 个有 5 个子孙的目录 → 维护 684 ms；维护前不变量 FAIL（孤儿 1、覆盖缺 4），维护后全部 OK；改名后段行 1、路径命中目录 5；删除目录残行 0；回滚后恢复原状。

## 4. snapshot 影响

- 大小：B 方案 plain +~290 MB（10.64→~10.9 GB，+2.7%），zstd 预计 +~25–30 MB（二元组压缩率与现有相近，按 959 MB×2.7% 估）；TTFB（首次 zstd 生成 44 s）预计 +~1–2 s。厂商表与 Vorbis 字段按实际导入量另计。
- schema 升级后旧 plain/zstd 快照立即作废（格式 ID 变），首个请求重新生成；建议迁移完成后由服务端**主动预生成** s9 zstd，避免首个客户端吃 44 s+。
- 生成前 `ensure_free_space(plain/8)`（`state.rs:322`）仍适用。
- **实测 s9→s8 派生（生产库副本）**：APFS clone 0.02 s → DROP 路径表/`vendor_metadata`（当时测的是旧逐文件表；目录表只有 2 万行，更快） + 删 meta + `user_version=8` 2.9 s → `VACUUM INTO` 109 s → zstd -3 12.5 s（页缓存热；bench 副本上冷 151 s）；**合计 125 s**。s8 快照 10.78 GB、zstd 945 MB，`quick_check=ok`，主 FTS 哈希与原 s8 相同。

### 4.1 双快照建议（过渡期）
- 推荐过渡期（默认 1 个版本周期，以 Paul 最终决定为准）**同时保留一份 s8 兼容快照**：由 s9 库派生（DROP `audio_dir_search`/`audio_dir_runs`/`vendor_metadata`，`source_search_rev` 回写 2，`user_version=8`），按需生成、只读、同一 library_id。成本：多一份 ~10.6 GB plain + ~959 MB zstd 磁盘，生成约 1–2 min。
- 发放规则：按 `x-audiolib-accept-snapshot-formats` 优先 s9，否则 s8；只接受 s8 的客户端在 `/capabilities` 与响应头 `X-AudioLib-Upgrade-Available` 收到升级提示。写操作只接受 s9 客户端（库实际格式 s9）。
- 注意 s8 派生快照里的 `bwf_description_tokenized` 已含 Vorbis/厂商文本（rev3 数据）— 旧客户端可读、可搜，只是不会解释新字段，属可接受。

## 5. 迁移成本实测（副本）

| 项 | 数值 |
|---|---|
| **目录段方案完整 8→9（生产库副本 `h.db`，`migrate9`，备份 API）** | backup 164.9 s（含 quick_check）/ path_build 0.81 s / optimize 0.20 s / switch <1 ms；总 166.1 s；不变量 OK；目录 FTS integrity ok；MG-10 主表哈希 `721d95f2320d3d2a` 不变 |
| 暂停/续跑（`i.db`，MAX_BATCHES=2） | 2 批后 `paused`，progress 10000/20248；续跑从 last_id=10000 继续，path_build 0.76 s，总 1.1 s，不变量 OK |
| 运行中 disk_full（`j.db`，第 3 批模拟 `SQLITE_FULL`） | `status=paused, error_code=disk_full`，退出码 5，checkpoint 保留（dirs/10000/10000），库仍 v8；续跑完成，不变量 OK |
| 只读（`k.db` chmod 444） | `status=failed, error_code=read_only, stage=preflight`，退出码 4；库仍 v8，无新表、无回滚文件 |
| 预检空间不足（模拟 1 GB < 13.8 GB） | `status=failed, error_code=disk_full, stage=preflight`，退出码 3，32 ms；库不动、无回滚文件 |
| 现有 FTS 全量重建（CLI `db rebuild-search`，含回滚备份 + integrity 校验，单事务） | **499 s**（user 162 s / sys 58 s，RSS 346 MB）；目录峰值 22.8 GB（库 10.4 GB → +12.4 GB = 回滚备份 10.6 GB + WAL ~1.6 GB） |
| 重建后库大小 | 10.64 GB（不变） |
| 同表 14 列新 FTS（一次性批插 5000/批） | 插入 243 s + optimize 20 s；+2049 MB |
| **20k 行/批可续跑回填（A 同表 14 列，最坏情况）** | 46 批，总 227 s；单批 min 0.84 / p50 4.18 / p95 12.08 / max 13.34 s；optimize 20.4 s；切换事务（RENAME×2）7 ms |
| 独立路径表 B（推荐） | 插入 64.6 s + optimize 2.4 s；+164 MB（未 VACUUM） |
| **B + t3（只路径表，bench 副本）** | 46 批 × 20k；插入 69.9 s（单批 p50 1.21 / p95 3.08 / max 3.46 s）+ optimize 2.7 s；共 82.7 s |
| **完整 8→9 原型（生产库 `.backup` 副本，`migrate9`）正常跑** | 回滚备份 `VACUUM INTO` + quick_check 229 s；回填 64.5 s（p50 1.14 / p95 2.78 s）；optimize 2.3 s；切换 2 ms；**总 296 s** |
| 暂停/续跑 | 20 批后暂停（29.2 s），续跑从 last_id=400000 继续 26 批 35.1 s，总 37.5 s |
| kill -9 + 续跑 | done=300000 时 kill -9（含备份共 243 s），checkpoint=300000，续跑 31 批 46.2 s |
| 预检拒绝（模拟可用空间 1 GB < 1.3×库 13.8 GB） | 32 ms 拒绝，退出码 3 `INSUFFICIENT_SPACE`；库保持 v8、无新表、无回滚文件 |
| MG-10 主表不变 | 原 s8 副本、正常/暂停/kill 三个 s9 结果、派生 s8 快照的 `audio_search_{data,idx,docsize,config}`+DDL 哈希全部 = `721d95f2320d3d2a` |
| 三种路径的结果一致 | 路径表行数 909,342 = audio_files；6 个查询命中计数三者完全一致；FTS `integrity-check` ok |
| trigram 路径表 | 15.2 s + 1.6 s；+167 MB |
| VACUUM（13.1 GB 库） | 327 s |
- 下表其余行为旧逐文件路径表方案的历史数据，仅作对照。旧方案 B 的迁移（不含备份）实测 ~67 s，含备份约 5 min（备份占 ~3.8 min，是大头）；加回滚备份（复制 10.6 GB，副本复制实测 28 s，SQLite 在线备份 + 校验约 2–4 min）总计 <5 min。p95 单批 12 s 意味着写锁最长阻塞约 12 s → 批大小建议自适应（目标单批 ≤2 s，B 方案下 20k 行约 1.4 s）。
- 不建议迁移内 VACUUM（327 s，且需再 1× 空间）；放到维护窗口可选。

## 6. 厂商 CSV/XLS(X) 导入 API（服务端）

输入事实（PR #208 附录 A）：449 张表（xls 209 / csv 127 / xlsx 103 + Soundminer txt 10），23 万行解析 0.8 s、峰值 <30 MB；20 万行 xlsx 0.79 s / 249 MB。

端点（manage 权限 + `X-AudioLib-Library-Id`，否则 409 `LIBRARY_ID_REQUIRED`）：
- `POST /api/vendor-import/preview`（multipart 文件或服务端路径 `root_relative_dir`）→ 返回 `import_id`、每文件结果、列映射建议、匹配统计、未匹配样例。**不写库**。
- `GET /api/vendor-import/{id}`（预览/进度）、`PUT /api/vendor-import/{id}/mapping`（改列映射/模板、选择 fuzzy、选择去重规则）。
- `POST /api/vendor-import/{id}/commit`（`Idempotency-Key` 头；同 key 重放返回同结果）→ 后台批写，每批 5k 行一事务，进度同 §2 格式。
- `POST /api/vendor-import/{id}/rollback` → 按 `import_id` 删除该批写入的 vendor_metadata 行并恢复被覆盖的旧值（写入时记录 before-image 到 `vendor_import_log`）。
- `GET /api/vendor-import` 列历史批次。

解析规则：
- 格式按 magic 判定（`D0CF11E0` = xls，`PK\x03\x04` = xlsx，否则文本），不信扩展名。
- 编码：BOM（UTF-16LE/BE、UTF-8）→ 严格 UTF-8 → GBK（GB18030）；记录检测结果。
- 分隔符嗅探 `,` / tab / `;`（前 64 KiB 统计一致列数）；Soundminer txt 走 tab。
- 跳过 `._*`、`~$*`、`@syno*`、`.DS_Store`；单文件失败不影响整体，记 `files[].error`。
- 表头在前 ~10 行内找（别名表命中最多的一行）；多 sheet 逐个处理。
- 上限（与 #208 测试计划统一）：单文件 50 MB（52,428,800 字节）、单文件 20 万行（xls 同样生效）、单行文本 256 KiB（262,144 字节，超出截断并计数）；超限文件整文件拒绝并在预览中显示。现状：服务端**还没有**导入代码；客户端 #213 只从 `/capabilities.vendor_import` 读 `max_file_bytes/max_rows`（测试夹具 52,428,800/200,000）；代码里唯一的 256 KiB 是 `metadata.rs:129` 搜索文本拼接上限。实测参考（#208 附录 A）：20 万行 xlsx 全量物化 249 MB / 0.79 s，csv 162 MB。
- 匹配：①厂商根目录（导入目录所在 root 子树）文件名索引；②全库文件名索引；③去扩展名匹配（`Foo` ↔ `Foo.wav/.flac`）；④可选 fuzzy（规范化空白/大小写/下划线，仅在预览中标“建议”，需用户确认）。相对路径列存在时优先路径精确匹配。多命中标“歧义”，不自动写。
- 去重/合并：同一文件来自多张表（多语言重复表）时，按 (文件, 字段) 合并：非空优先 → 预设语言优先（zh > en > 其他，可配置）→ 后导入覆盖需显式选择；keywords 求并集去重。
- 写入：`vendor_metadata(audio_file_id, import_id, field, value, source_file, sheet, row)`，并重算该行 `bwf_description_tokenized` 对应的派生搜索文本（走触发器更新 FTS）；`source_search_rev` 不变（rev3 已包含 vendor 组）。

## 7. Vorbis/FLAC/OGG（服务端）
- 历史提案曾将扩展白名单与默认值留待决定；当前扫描默认支持 WAV、FLAC、Ogg Vorbis 和 OGA/Ogg FLAC，symphonia 已启用 `flac,ogg,vorbis`。
- 历史提案担心旧端收到非 WAV 后解码失败；当前 s8 兼容派生快照已强制过滤非 WAV 行，旧端不会收到这些新格式记录。接受原生 s9 的新客户端可同步完整来源。
- 已有 wav 库无需重扫；FLAC/OGG 需要一次增量扫描（管理员触发）。

## 8. 服务端需要暴露的客户端提示触发点
1. `/capabilities.compatibility.features` 含/不含 `search_path|vorbis_tags|vendor_metadata` → 客户端显示/隐藏功能。
2. `/capabilities.schema_migration.state` + `/health.upgrade` → “服务器正在升级库（x%）”。
3. 409 `SNAPSHOT_FORMAT_UNSUPPORTED` / 版本类 409 → “请升级客户端”。
4. 只拿到 s8 兼容快照时：`/capabilities.client_upgrade` + 响应头 `X-AudioLib-Upgrade-Available`（§1.1.1）→ 非阻塞提示；deferred 时 `X-AudioLib-Schema-Migration: deferred; target=9`。
5. 新快照可用（`snapshot_id` 变化）→ 客户端提示刷新。
6. 厂商导入完成 / 回滚（`/api/vendor-import` 状态）→ 刷新快照提示。

## 9. 服务端视角的边界情况
- 迁移中崩溃/断电：checkpoint 续跑；切换前失败即回到 s8，无需还原。
- 迁移中扫描/写入：触发器先装，回填幂等（delete+insert）。改名/移动/删除：按 §3.5 同事务维护目录表。
- 磁盘不足：预检 `failed/disk_full`、库不动；运行中 `SQLITE_FULL` → `paused/disk_full`，保留 checkpoint。只读：预检 `failed/read_only`，库不动。
- 超长路径（>4 KiB）截断并计数；NFD 路径（macOS）统一 NFC 后再分词。
- 根名/共享前缀导致超高频路径词：根名不入索引 + 命中超过一半文件即丢弃路径集（§3.2）。
- 同一库多个服务端实例（<prod-port>/<beta-port>）共用 DB：迁移需独占 upgrade lease（已有 `*.upgrade.lock`）。
- 回滚备份磁盘占用：保留，管理端只提示不自动删除。
- 旧客户端写请求打到 s9 库：按库实际格式 409 拒绝。

## 10. <prod-port> 上线计划（每一步都需 Paul 批准）
1. beta 渠道 + <beta-port> 之外的新测试实例用副本跑完整迁移与回归（不碰 <beta-port>、PR #97）。
2. <prod-port> 停机窗口前：确认 <data-volume> 余量 ≥ 库×1.3（当前 4.4 TB 空闲，充足）；手工额外备份（不删任何旧备份）。
3. 升级二进制（launchd `<launchd-label>`）→ 启动时自动进入后台迁移（或设 `AUDIOLIB_DEFER_SCHEMA9=1` 由管理员按钮触发，推荐后者）。
4. 观察 `/admin/api/migration`：预计 <5 min；完成后预生成 s9 + s8 兼容 zstd。
5. 验收：固定查询集 p50/p95 对比基线不升高；`integrity-check`；老客户端可读 s8；新客户端出现路径范围开关。
6. 回滚预案：切换前取消即可；切换后停服 → 用 `audiolib-rollback-*.db` 还原 → 回旧二进制。

## 附：基准原始数据
`<bench-dir>/run7.log`（目录段方案：建表、maint-test、20 条查询 S8ORDER/oracle/翻页、取消、5 轮热测各轮 p95）、`mig/run8.log`（目录段方案生产副本迁移 + 暂停/disk_full/read_only/预检）、`cold.sh`（OS 冷态，Paul 运行）；基准程序 `<bench-worktree>/crates/audiolib-db/src/bin/search-b-bench.rs`（未提交）。旧逐文件方案数据：`run.log`–`run4.log`、`mig/run5.log`、`mig/run6.log`、`mig/mg10.log`。
