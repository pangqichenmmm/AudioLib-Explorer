# Search Metadata v2 边界用例清单与回归测试计划

- 基线：v0.8.7 / commit b5a98ff / schema 8；feat 分支 `feat/search-metadata-v2`（自 beta 4ff16a5）
- 关联 issue：#204 路径搜索、#205 Vorbis/FLAC/OGG、#206 厂商 CSV/XLS 导入、#207 总跟踪
- 优先级：P0 = 阻塞合入 feat 分支；P1 = 阻塞合入 beta；P2 = 可后续跟进
- 对齐：设计 PR #209 @ cddc7e3（`doc/DESIGN-SEARCH-METADATA-V2.md` §10.7）、服务端设计稿（2026-11-03 评审），以及目录段路径搜索 #216 @ `b3fd304`（已 squash 合入 `feat/search-metadata-v2`）。格式常量 `PLAN_B_V9_FORMAT = "ale-s9-cld1-t3-m2-p1-a1-sd1"`
- 验证方式：U = 单元测试，I = 集成测试（临时 DB/样本库），C = CLI/脚本，G = 手工 GUI

## 0. 现状代码路径（用例锚点）

| 位置 | 说明 |
|---|---|
| `crates/audiolib-db/src/schema.rs:17` | `SCHEMA_VERSION = 8`，`MIN_SNAPSHOT_SCHEMA_VERSION = 3`（:21） |
| `schema.rs:~1307-1311`、`lib.rs:~2837-2840` | 基础搜索 = 单个 FTS `MATCH`，字段为文件名、用户标签、BWF description、iXML scene/take、notes 及分词变体；路径不入索引，文件夹只做过滤 |
| `schema.rs:1075`、`audiolib-core/src/metadata.rs:94-127` | UCS CatID/Category/SubCategory/VendorCategory 拼接进 description token |
| `wav.rs:218-238` | ID3（WAV 内 text、COMM、TXXX）提取 |
| `remote_mode.rs:114/156` | 服务器模式打开只读快照，执行同一套 SQL |
| `lib.rs:151` | 查询被中断时返回 `OperationInterrupted`，取消逻辑依赖它 |

规模基线：约 90.9 万文件，DB 约 10.6 GB，快照压缩后 959 MB。

---

## 1. P0 性能门禁（Performance Gate）

**规则：只要有一项超出门禁，就不能合入 `feat/search-metadata-v2`。** 允许多用 CPU，但延迟不能变长。基准数据由 server bot 用 8790 生产库的**副本**提供（不碰生产服务）。路径搜索按目录段索引实现（#216）：FTS 表 `audio_dir_search`（每个目录一行，rowid = `folders.id`，只存本段目录名）和 `audio_dir_runs`（同一目录下连续文件 id 段）。主表 `audio_search` 不加路径列（列结构和内容都不变，迁移不重建主表），路径不参与主表 bm25。`scope` 省略或 `main` 时排序与 schema 8 完全一致；分层排序只在 `scope=all` 与 `scope=path`。旧的逐文件表 `audio_path_search` 不再建。

冷缓存的做法：重启进程，并清 OS 文件缓存（Windows 用 RAMMap `EmptyStandbyList`，Linux 用 `echo 3 > /proc/sys/vm/drop_caches`，macOS 用 `purge`）。热缓存：同一查询先预跑 3 次，再计时。

**统计方法：** 每条查询的 p95 = 独立跑 5 轮、每轮各算一个 p95，取这 5 个 p95 的中位数（p50 同理）。门禁判定口径（Paul 2026-11-03 决定）：上述 5 轮 p95 的中位数 ≤ schema 8 同一查询的对应值 × 1.2（最多慢 20%）即通过，否则不通过。下文所有「≤ schema 8」的延迟门禁都按此口径判定。

| ID | 场景 | 预期 / 门禁 | 验证 | 优先级 |
|---|---|---|---|---|
| PG-01 | 固定查询集：中文、英文、1–2 字符短词、路径片段、UCS CatID、多词组合，每类 ≥10 条（设计 §10.6 每类 25 条） | schema 9 的 p50/p95 ≤ schema 8 × 1.2；冷缓存和热缓存各测一次 | C：bench 脚本，同机同库副本，每条跑 ≥30 次 | P0 |
| PG-02 | 「仅路径」范围（`scope=path`：只查 `audio_dir_search`，文件经 `audio_dir_runs` 展开；不含纯主表命中） | p95 ≤ schema 8 文件名查询的 p95 × 1.2 | C | P0 |
| PG-03 | 路径搜索的 SQL 计划 | `EXPLAIN QUERY PLAN` 不能出现针对 path 的 `LIKE '%..%'` 全表扫描，必须走 `audio_dir_search` 的 FTS | U：对生成的 SQL 断言执行计划 | P0 |
| PG-04 | Vorbis/厂商字段的接入方式（暂定：合并列） | Vorbis 和厂商文本都并入现有的 `bwf_description_tokenized`，主表 `audio_search` 不新增列，仍然一次 MATCH；搜索不 JOIN `vendor_metadata`（执行计划和 schema 8 对比） | U：执行计划断言 | P0 |
| PG-05 | 后台导入 CSV、重新提取 Vorbis 或目录改名重算路径时并发搜索 | 搜索 p95 不超过空闲时 p95 的 1.1 倍；写事务分批提交，读不被阻塞（WAL） | I：边写边跑 bench | P0 |
| PG-06 | 快速连续输入（防抖），以及切换「全部 / 仅文件名 / 仅路径」 | 防抖窗口内只发一次查询；新查询（包括切换范围）发出时取消上一次（走 `OperationInterrupted`，lib.rs:151），不出现旧结果覆盖新结果 | U + G | P0 |
| PG-07 | 索引体积增长（主 FTS + `audio_dir_search` + `audio_dir_runs`） | 记录 DB 以及这两张目录表前后的大小；**增长 ≤15%**，超出即不通过。旧逐文件表的 +14.2% 只作历史对照，不作为本索引的预期 | C | P0 |
| PG-08 | 快照大小与首字节时间 | 记录压缩快照大小和服务器首字节时间；首字节时间不能变长 | C：curl 计时 | P0（必须记录） |
| PG-09 | 服务器模式下同样的查询集 | 远程 p95 ≤ schema 8 远程 p95 × 1.2 | C | P0 |
| PG-10 | 「全部」范围（`scope=all`，热缓存）：主表与目录段索引合并 | p95 ≤ schema 8 同一查询集的 p95 × 1.2；同时记录分层合并的额外耗时（设计 §10.1 估计 2–6ms） | C：查询集 ×30 次，记录 p50/p95/max | P0 |
| PG-11 | 「全部」范围（`scope=all`，冷缓存，清 OS 文件缓存） | p95 ≤ schema 8 冷缓存 p95 × 1.2；目录段索引首次载入的耗时单独记录 | C：每轮清缓存后重跑 | P0 |
| PG-12 | 合并排序与去重的正确性 | 同一文件在主表和目录段都命中时只出现一次；主表（文件名/描述）命中排在只命中路径的前面；同分按文件 id 稳定排序；结果与参考实现的 ID 序列完全一致 | U + I：已知样本库 + 随机查询对比 | P0 |
| PG-13 | keyset 翻页（OFFSET > 2,000 时用游标；游标覆盖主表层、混合层和仅路径层） | 库不变时逐页取完 = 一次取全集，不漏不重；覆盖层交界、同分并列及某层先取完。查询、范围、条件、排序、库内容或 provider 会话变化后旧游标须以 `CURSOR_STALE` 拒绝，客户端丢弃旧续页、从第一页重取，不混合两代结果 | U + I：每页 200 条遍历 ≥5 万条结果，并注入变更 | P0 |
| PG-14 | 「全部」或「仅路径」查询时取消 | 搜索返回 `OperationInterrupted`；取消后没有残留的语句或事务；旧结果不会显示出来 | U + I：在长查询途中取消，检查连接状态 | P0 |
| PG-15 | 「仅文件名」范围（主表列过滤 `{filename filename_tokenized}`） | p95 ≤ schema 8 文件名查询的 p95 × 1.2 | C | P0 |
| PG-16 | 高频路径片段查询（根目录名、命中 >5 万行的路径片段），作为**必测门禁查询**并入 PG-01/PG-10 查询集。目录命中集为空时走产品代码里的主表 `LIMIT 1` 探空短路（#216，见下；过闸数字来自 #215 的 bench） | p95 ≤ schema 8 同一查询 p95 × 1.2（口径见上文统计方法）；热测过闸例：`hdd_A_Lib explosion` 5 轮 p95 中位 s8 **5.18** ms / s9 **4.78** ms，比 **0.922** ≤ 1.2 → **PG PASS**（此前曾复现 4.6ms → 649ms 的回归，修复+短路前不得合入） | C：每个库至少取 5 条根目录名和 5 条命中 >5 万行的片段 | P0 |
| PG-17 | keyset cursor 分页：请求带 `cursor`，响应回 `next_cursor`；可与 offset / page 共存，同时传时 cursor 优先 | 有下一页时返回非空 `next_cursor`；末页 `next_cursor` 为 `null`；cursor 对客户端不透明；与 offset/page 同时传时以 cursor 为准 | I | P0 |


**目录命中集为空的短路（产品代码 #216，2026-11-03）**：某个查询词在 `audio_dir_search` 的目录命中集为空时，先用该词对主表 FTS 做 `LIMIT 1`（`SELECT rowid FROM audio_search WHERE audio_search MATCH ? LIMIT 1`）。主表也没有命中则直接返回空页，**不再**跑全量主扫描。这是 #216 的产品行为，不是只在 bench 里。#215 的 bench 测过本例 `hdd_A_Lib`：主表 0 行，约 4.3 ms 证空，正确性仍为 0 行。短路不得漏掉「目录集为空但主表有命中」的词（见 PS-24）。

---

## 2. 路径搜索（#204，方案 B）


**接口约定（2026-11-04 修正范围歧义）**：`SearchRequest.scope=main|all|path|filename`；**不传 scope 等于 `main`**，保留 schema 8 的主字段命中与排序。UI「主字段」对应 `main`，「仅文件名」对应独立的 `filename`，「全部」对应 `all`，「仅路径」对应 `path`。`filename` 限定文件名列；`path` 要求每个词都命中目录或祖先目录，文件名/Metadata 不补足路径词；跨来源 AND 只用于 `all`。分层排序（PS-23）只在 `all` 下形成三层；`main`/缺省不走分层。客户端在经过校验的远程快照上执行相同查询；当前没有在线 HTTP 搜索端点。路径索引是 `audio_dir_search` 加 `audio_dir_runs`，不是逐文件的 `audio_path_search`。

| ID | 场景 / 输入 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| PS-01 | 路径长度 >260、带 `\\?\` 前缀 | 目录段写入 `audio_dir_search`（只存本段目录名），文件经 `audio_dir_runs` 解析后能搜到；`\\?\` 前缀不产生 token；搜索结果和打开文件都正常 | U：分词；I | P0 |
| PS-02 | UNC 路径 `\\nas\share\SFX\...` 和映射盘 `Z:\` | 服务器名、共享名按段切分；同一文件不因 UNC 和盘符两种写法在路径结果里出现两行（文件只经一条 `audio_dir_runs` 段解析） | U + I | P1 |
| PS-03 | 中文、日文、emoji 路径段（如 `音效库/爆炸`、`効果音`、`🔥fire`） | 中文用 jieba 词典分词能搜到（不用 bigram）；emoji 不导致崩溃或乱码 token | U | P0 |
| PS-04 | 分隔符混用 `a\b/c`、连续分隔符、末尾分隔符，以及 `_ - . () [] {} & + ,` | 按设计 §2.1 统一替换成空格，切出的段一致，不产生空 token | U | P0 |
| PS-05 | 大小写（`Foley` 和 `foley`）、全角、驼峰 | 不区分大小写；NFC、全角折叠和驼峰拆分与文件名列一致 | U | P1 |
| PS-06 | 文件或文件夹重命名、移动后重新扫描 | 扫描写库的同一事务里维护目录段：改名重写 `audio_dir_search` 中该目录 1 行；移动目录时段名不变则不重写段索引，祖先关系随 `folders` 现值重载；文件增删或跨目录移动重算相关 `audio_dir_runs`。旧路径词搜不到，新路径词能搜到；`audio_dir_search` 和 `audio_dir_runs` 都没有残留行 | I | P0 |
| PS-07 | 文件夹过滤加路径关键词，且关键词就是过滤目录名 | 结果是两个条件的交集，不重复；过滤语义和 schema 8 相同 | I | P0 |
| PS-08 | 排序：「全部」范围下，查询词同时命中文件名和路径 | 主表命中排在只命中路径的前面；整个目录都命中时，前 50 条结果不能全被只命中路径的行占满 | I：在已知样本上断言排名 | P0 |
| PS-09 | 盘符、通用段（`X:`、`Users`、`SFX`、`Sound Effects`、`音效`、`素材`） | 盘符不入索引；静态停用词表里的词不产生 token；根目录名默认不入索引，开关打开后才入 | U | P1 |
| PS-10 | 路径中有 FTS 语法字符（`"`、`*`、`-`、`(`、`:`、`NEAR`） | 主表和 `audio_dir_search` 的查询都正确转义，不报 FTS 语法错误 | U | P0 |
| PS-11 | 范围切换「全部 / 主字段 / 仅路径 / 仅文件名」；scope 为 `all/main/path/filename`，省略仍为 `main` | 主字段保留 s8 的文件名、标注和内嵌 Metadata 搜索；仅文件名只查文件名列；仅路径的每个词都须命中目录；全部允许跨来源 AND；以英文、中文和数字词验证集合、分页、筛选及排序，本地与 HTTP 同步后的快照一致 | I | P0 |
| PS-12 | `audio_dir_search` / `audio_dir_runs` 与目录、文件一致 | `audio_dir_search` 的 rowid = `folders.id`，行数 = `count(folders)`（`contentless_delete=1`）；`audio_dir_runs` 无孤儿行，`sum(n) = count(audio_files)`，没有 run 含别的目录的文件；删目录后段行和 runs 都没有残留；integrity-check 通过 | U + I | P0 |
| PS-13 | 目录本身改名（`folders.name` 变化） | 后台任务按 `folder_closure` 找后代文件，每批 5,000 行更新；不在 UI 线程执行；中途 kill 后按 `library_meta.path_reindex_progress` 续跑，最终结果和一次跑完一致；期间搜索不被阻塞（PG-05） | I | P0 |
| PS-14 | 主表结构不变 | `audio_search` 的列、tokenizer 和选项（contentless、`detail=full`、`columnsize=1`）都和 schema 8 相同，没有路径列，也没有厂商列；迁移前后主表不重建，token 不变（主表不做 NFC/全角半角归一化）；只出现在路径里的词在「仅文件名」范围下搜不到 | U：schema 断言 | P0 |
| PS-15 | 新客户端连旧服务器（`/capabilities.compatibility.features` 里没有 `search_path`） | 隐藏「仅路径」和厂商相关的范围开关，「全部」退化成只查主表，不报错 | I | P1 |
| PS-16 | All 范围多词跨表（如 `pentax shutter`，一个词在文件名里、一个词在路径里） | `scope=all` 按词计算：每个词取（主表 ∪ 目录段）的命中，再对所有词求交集；结果等于参考实现；任意一个词两边都不命中时，结果为空。`scope=path` 严格要求每个词均命中目录，不用主字段补足 | U + I | P0 |
| PS-17 | 超高频路径词（共享前缀、`Pro Sound Effect` 之类） | 路径词估计命中文件数超过全库一半时丢弃该词的目录集，只走主表；估计 ≤5 万时只扫描子树内的 `audio_dir_runs`。p95 仍满足 PG-10；根名不入索引时，搜根名不会命中全库 | I + C | P0 |
| PS-18 | 超长路径（>4 KiB）和 macOS NFD 路径 | 超长路径截断后入索引，截断次数计入统计或日志；NFD 先统一成 NFC 再分词，NFC 和 NFD 写法搜到的结果相同 | U | P1 |
| PS-19 | `audio_dir_search` 只存本段目录名 | 目录段 token 不包含文件名本身；只出现在文件名里的词，在「仅路径」范围下搜不到 | U | P1 |
| PS-20 | 「全部」范围多词查询，词分布在不同表 / 同一表：(a) `door` 只在路径、`slam` 只在文件名；(b) 两个词都只在路径；(c) 两个词都只在文件名 | 三种情况都命中该文件（按词取主表与目录段的并集再求交，见 PS-16）；不因跨表而漏掉；同一文件只出现一次 | U + I：已知样本断言 | P0 |
| PS-21 | 目录表方案下的多词跨表：一个词只在目录名里（目录表命中），另一个词只在文件名里（如目录 `Door`、文件 `slam_01.wav`） | 该文件命中且只出现一次；先按目录表命中的 `dir_id` 过滤文件、再与主表命中合并时不漏掉该文件；路径词不参与 bm25 排序，只做过滤；结果等于参考实现（同 PS-20 的按词并集再求交） | U + I：已知样本断言 | P0 |
| PS-22 | 目录改名 / 移动 / 删除，分别走全量扫描和增量扫描 | 目录表和文件的 `dir_id` 映射同步更新；旧路径词搜不到、新路径词能搜到；没有漏掉的文件，也没有重复行；删除目录后，其下文件不再出现在路径搜索结果里，目录表无残留行 | I | P0 |
| PS-23 | 分层排序与服务端设计 §3.2 统一：`all` 为主字段完整命中 > 混合命中 > 纯路径命中；第0层按主表 bm25、ID，混合和纯路径层按 ID；严格 `path` 只含路径集合。`main`/缺省保持 s8 顺序。显式非相关性排序作用于完整命中集合 | 用不同主字段分数构造混合命中参考序列，断言其按 ID；第0层及 `main` 与 s8 **逐条一致**。覆盖全量、筛选、各排序和分页；游标过期须明确拒绝，不重放旧页 | I：对比结果 ID 序列 | P0 |
| PS-24 | 目录命中集为空的短路（产品代码 #216；bench 过闸见 #215）：(a) 该词在 `audio_dir_search` 无命中且主表 `LIMIT 1` 也无命中；(b) 目录集为空但主表有命中 | (a) 返回空页（0 行），跳过全量主扫描；(b) **不得**因短路漏掉主表命中，仍应返回主表命中（集合与不短路参考实现一致） | U + I：构造目录空/主表空与目录空/主表有两组样本 | P0 |
| PS-25 | 路径结果超过一页，任务/收藏在不同 scope 间切换，再恢复；远程删除覆盖层 | 任务保存各自范围，旧任务缺字段默认 Main；`total_count_exact=false` 显示至少/≥，不按下界 clamp 或关闭下一页。游标位置优先于请求 offset；末页确认精确总数；返回上一页和刷新不混用旧游标，删除覆盖层不冒充精确总数 | U + I + G | P0 |
| PS-26 | 超过256层的合法目录、稀疏文件 ID 和 NULL folder_id | 原生及带无损筛选的 Path/All 集合和 facets 一致，祖先匹配不截断；路径覆盖阈值按真实 run 成员和全库记录计算，不信任陈旧目录统计；无归属 ID 在最后 run 后不崩溃 | U + I | P0 |

## 3. Vorbis / FLAC / OGG 标签（#205）

当前候选已接通默认 `wav/flac/ogg/oga` 扫描、真实格式 probe、FLAC/Vorbis 解码和 seek，
并复用注册的 `ByteSource`（包括无真实文件的流身份和 `.wav` 缓存名）。以下仍是验收断言，
本段不把新增实现或测试文件记为已通过，也不代替跨平台、真实 HTTP Range 和 PG 性能验收。

提取器将原文放在 `wav_metadata.vorbis`，键转为大写，重复值保留为多值；规范映射放入
`wav_metadata.fields`。映射键的白名单限制规范字段，不删除未知 textual key 的原文；原文也进入
现有 Metadata 搜索投影，图片键/二进制例外。提取预算为 comment 1 MiB、最多 4096 项、
每个保留值 16 KiB；越界/截断记录警告，安全读出的前缀可保留，无效 UTF-8 有损替换。
`vorbis_version=1` 参与增量扫描 fingerprint；旧提取版本会重新读取，用户标注、未知扩展和独立厂商镜像保留。

Opus 目前仅支持 OpusHead/OpusTags probe，不默认扫描 `.opus`，没有 bundled Opus decoder，
播放须明确返回 unsupported。Ogg Vorbis、Ogg FLAC 与链式 Ogg 标签按第一条逻辑流提取；
不能将 VB-07 的“都能提取”解释成所有 Ogg codec 都能播放。播放层虽传入 tag/visual 限额，
Symphonia 0.6.1 embedded readers 的恶意长度保护还依赖 FLAC ScopedStream/物理块和 Ogg 16 MiB packet 边界，
不能把提取器 1 MiB 预算写成总播放内存上限。

| ID | 场景 / 输入 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| VB-01 | FLAC metadata block 损坏或被截断 | 跳过该文件的标签提取并记录警告，不 panic，扫描继续 | U：fuzz 样本 | P0 |
| VB-02 | 超大 comment block（如 16 MB） | 有大小上限，超出部分截断；内存占用有界 | U | P0 |
| VB-03 | 同一个 key 出现多次（如多个 ARTIST） | 所有值都保留并能搜到 | U | P1 |
| VB-04 | 值不是 UTF-8 | 有损解码，不丢整条记录、不 panic | U | P0 |
| VB-05 | 值为空、只有 `KEY=`、没有 `=` | 忽略，不产生空 token | U | P1 |
| VB-06 | 带嵌入图片（METADATA_BLOCK_PICTURE / FLAC PICTURE） | 不进 FTS，不把 base64 当文本 | U | P0 |
| VB-07 | Opus（OpusTags）、Ogg Vorbis、OGG 封装的 FLAC、链式 OGG | 都能提取；链式 OGG 至少取第一段；另外确认 Opus 仅标签支持、播放明确 unsupported，不扩大 codec 承诺 | U + I | P1 |
| VB-08 | 未知 key、key 大小写混用 | key 统一大小写；规范字段按映射白名单，未知 textual key 原文保留；图片键仍排除（边界见上） | U | P1 |
| VB-09 | 已入库的 FLAC/OGG 文件需要重新提取 | 有触发机制；重提取在后台运行，不阻塞搜索（见 PG-05） | I | P0 |

## 4. 现有元数据回归

| ID | 场景 / 输入 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| EM-01 | BWF bext 损坏、长度字段越界 | 和 schema 8 行为一致，不 panic | U | P0 |
| EM-02 | iXML 格式错误或编码错误 | 跳过 iXML，其他 chunk 照常提取 | U | P0 |
| EM-03 | ID3 COMM/TXXX 损坏（wav.rs:218-238） | 不 panic | U | P0 |
| EM-04 | GBK、Shift-JIS、Latin-1 编码的 INFO/bext 文本 | 解码结果和 schema 8 一致（不能新增乱码） | U：golden 样本 | P1 |
| EM-05 | 超大 chunk（单个 chunk ≥100 MB 的声明长度） | 有上限，不 OOM | U | P0 |
| EM-06 | 基线命中数（r824 快照）：`zildjian`=323、`electrovoice`=228、`sortilege`=2776、`personage`=5943 | schema 9 的命中数 ≥ 基线，且基线命中的结果全部还在（新增结果只能来自路径或 Vorbis 字段，并逐条列出） | C：命中数脚本 + ID 集合对比 | P0 |
| EM-07 | UCS CatID/Category 拼接（schema.rs:1075，metadata.rs:94-127） | token 不变，按 CatID 搜索的命中数一致 | U + C | P0 |
| EM-08 | 用户标签、notes 搜索 | 和 schema 8 一致 | I | P0 |
| EM-09 | 全角文件名样本（如 `ＥＸＰＬＯＳＩＯＮ_０１.wav`、半角片假名）、NFD 文件名样本（macOS 写入的 `café`、`ガ` 分解形式） | 1）**主表**：主表不做归一化，对每个样本查询，s9 主表和 s8 主表的命中数和 ID 集合完全相同（不多也不少）。2）**路径搜索**：`audio_dir_search` 和发往它的查询做 NFC 加全角/半角归一化，用 NFC 或 NFD、全角或半角任意一种写法输入，「仅路径」都能搜到所有变体；「全部」范围里多出的变体命中只能来自目录段索引 | U：路径归一化 golden；C：s8/s9 主表命中数和 ID 集合对比 | P0 |

## 5. 厂商 CSV/XLS 导入（#206）

暂定决定（Paul 可能调整）：
- **存储**：厂商文本并入 `bwf_description_tokenized`，主表的列结构不变（主表不重建，厂商文本通过增量更新写入）。代价是厂商文本没法单独设权重。原始数据分开保存：厂商数据在 `vendor_metadata`，内嵌数据在原来的字段（BWF/iXML/Vorbis）里。
- **优先级** 用户 > 厂商 > 内嵌 **只决定显示哪一个**；搜索时厂商和内嵌两种来源都能搜到。
- **一行对应多个文件**：标为「歧义」，在预览里经用户确认前不自动写入。
- **一个文件对应多行**（例如多语言重复表）：按（文件, 字段）合并，非空的值优先；keywords 取并集并去重。
- **显示语言**：中文优先，没有中文时用英文。

手动流程：`POST /api/vendor-import/preview` →（`PUT …/{id}/mapping`）→ `POST …/{id}/commit`（带 `Idempotency-Key`）→ 必要时 `POST …/{id}/rollback`。所有接口都需要 manage 权限和 `X-AudioLib-Library-Id`。

用户追加的自动流程纳入 #206：本地客户端/CLI 或服务端扫描 WAV 时，同时发现同目录 CSV/TSV/XLS/XLSX，
在 WAV 批次写入后识别表头、匹配并合并 Metadata。自动匹配只接受表格同目录下唯一且本轮扫描实际确认的 WAV，
不使用全库同名、stem 或 fuzzy 回退；不把外部绝对路径、父目录或子目录路径降为同名匹配。
手动映射、跨目录选择和歧义确认继续使用上述手动流程。

| ID | 自动扫描场景 | 预期 | 验证 | 优先级 |
| --- | --- | --- | --- | --- |
| AS-01 | 本地扫描目录内真实 WAV + CSV/BIFF8 XLS/OOXML XLSX | 自动识别各 sheet 的表头，同目录唯一 WAV 得到新增可搜字段；源音频与表格字节不变 | I + G | P0 |
| AS-02 | 内嵌 Metadata 与表格值相同/部分重复，含大小写、空白、Unicode、keyword 列表差异 | 显示及搜索投影去重，保留原始来源；不同值保留；用户标签、备注、评分、收藏和颜色不变 | U + I | P0 |
| AS-03 | 相同内容重复扫描；仅表格变化；表格不变而新增 WAV；内嵌值改变 | 幂等；无需强制重提取 WAV 即可更新；只替换该表格自己的贡献，保留其他表格和手动批次 | I | P0 |
| AS-04 | 多目录同名 WAV、同目录不唯一的别名、外部/穿越路径、非 WAV；未关联表格插行/重排 | 不跨目录或近似匹配，不误写；歧义和未匹配计数可见，允许改用手动导入；部分关联时只合并新值并保留该表全部旧贡献，完整匹配后才清理自身过时值 | I | P0 |
| AS-05 | 损坏/无匹配列/超限/不可读表格、表格消失或 NAS 掉线 | 可诊断且保留最后一次有效 Metadata；不删除源文件、不用失败输入抹掉旧来源 | I | P0 |
| AS-06 | 解析、提交或暂停时取消/关闭；重复扫描恢复 | 完整解析后才写；提交有界且事务原子，已提交来源保留，重扫不重复；扫描锁及关闭排空契约继续成立 | I + G | P0 |
| AS-07 | 服务端 s8 与真实迁移后 s9 自动扫描；只改表格并同步 | 权限与库身份校验继续适用；revision/快照更新，新客户端同步及离线重开可搜，过渡期 s8 快照保留 WAV 新文本 | HTTP + I | P0 |
| AS-08 | 大目录遍历与并发搜索 | 发现表格使用有界队列和磁盘 spool，不把所有路径或行收进内存；复用分批缓存维护及 PG05 门槛 | I + P | P0 |

AS 用例属于新增验收范围，执行结果须绑定实际源码和包；此前手动 CX 通过不能替代它们。

上限：单文件 50 MB（`/capabilities.vendor_import.max_file_bytes = 52428800`）、20 万行（`max_rows = 200000`）、单行文本 256 KiB。测试按这些值的边界 ±1 来做，并断言 capabilities 返回的就是这两个数。

| ID | 场景 / 输入 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| CX-01 | 空文件 | 提示“无数据”，不写库 | U | P0 |
| CX-02 | 只有表头 | 同 CX-01 | U | P1 |
| CX-03 | 缺少文件名/路径列，或列被改名 | 进入列映射界面（`PUT mapping`）；缺少匹配键时禁止 commit | U + G | P0 |
| CX-04 | UTF-8 BOM、UTF-16 LE/BE、GBK/GB18030 | 检测顺序为 BOM → 严格 UTF-8 → GB18030，检测结果写进预览；也可以手动选择；预览不乱码 | U | P0 |
| CX-05 | 分隔符为 `,`、`;`、`\t`、`\|` | 在前 64 KiB 内按列数一致性嗅探，不看扩展名；可以手动覆盖 | U | P1 |
| CX-06 | 引号内换行、转义引号 | 按 RFC 4180 解析 | U | P0 |
| CX-07 | 以 `=`、`+`、`-`、`@` 开头的单元格（公式） | XLS 读缓存值，不执行公式；导出时防公式注入 | U | P1 |
| CX-08 | 合并单元格 | 按左上角的值处理，或在预览中给出警告 | U | P2 |
| CX-09 | 多个 sheet | 每个 sheet 单独找表头、单独处理，预览按 sheet 列出结果 | U + G | P1 |
| CX-10 | 10 万行以上（以及 100 万行压力测试） | 流式解析；每批 5,000 行一个事务；有进度；不阻塞搜索（PG-05） | I + C | P0 |
| CX-11 | 一行匹配到多个文件（同名文件在不同目录） | 标为「歧义」，commit 时**不自动写入**；用户在预览中确认后才写；预览列出所有候选 | I + G | P0 |
| CX-12 | 行匹配不到任何文件 | 计入未匹配，预览给出样例，可以导出报告 | I | P1 |
| CX-13 | 按文件名匹配和按相对路径匹配；大小写和分隔符差异 | 有相对路径列时优先按路径精确匹配；之后依次是厂商根目录文件名索引 → 全库文件名索引 → 去扩展名；规范化规则与 PS-04、PS-05 一致 | U | P0 |
| CX-14 | 导入值和用户标签冲突 | 用户标注优先，永远不覆盖用户列；预览中显示冲突 | I + G | P0 |
| CX-15 | 同一份文件重复导入 | 幂等：`bwf_description_tokenized` 里不会出现重复的厂商文本，token 计数不变 | I | P0 |
| CX-16 | 回滚一次导入 | 精确恢复导入前的状态（包括 FTS），不影响其他批次 | I | P0 |
| CX-17 | 导入中途取消或进程被杀 | 已提交的批次可以回滚或续导；不留下半批数据 | I | P0 |
| CX-18 | 服务器模式下的客户端 | 只有 `can_manage` 为真时才显示导入入口；不是 s9 的客户端发写请求会被 409 拒绝 | G + I | P1 |
| CX-19 | 单文件超过 `max_file_bytes`（csv、xlsx、**xls** 各测一次，取上限 −1 / 0 / +1 字节） | 超限的文件整文件拒绝，在加载内容前就判断；预览说明原因；不写库 | U + I | P0 |
| CX-20 | 行数超过 `max_rows`（上限 −1 / 0 / +1，包括多 sheet 合计） | 超限的文件整文件拒绝，预览说明原因；不能部分导入 | I | P0 |
| CX-21 | xls（BIFF）接近上限的文件 | calamine 读 xls 会整个 sheet 进内存，所以先按大小拒绝；在上限以内的最大 xls 记录峰值 RSS，不 OOM | I + C：记录 RSS | P0 |
| CX-22 | 流式解析时的内存（xlsx 走 `worksheet_cells_reader`，csv 逐行读） | 接近上限的 csv 和 xlsx 各记录一次峰值 RSS，内存不随行数线性增长；附录 A 全量物化的数字（20 万行 xlsx 249 MB / csv 162 MB）作为对照上界 | C | P0（必须记录） |
| CX-23 | 单行文本超过 256 KiB（262144 字节 ±1） | 按设计截断或拒绝该行，计数后显示在预览中；不 panic | U | P0 |
| CX-24 | preview 不写库 | 跑完 preview 后 DB 文件内容和 `library_meta` 不变；`GET /api/vendor-import/{id}` 能取回预览结果 | I | P0 |
| CX-25 | `Idempotency-Key` | 同一个 key 重放 commit，返回同一个结果，不会写第二遍；同一个 key 但 mapping 不同，返回错误；没有 key 时拒绝 commit | I | P0 |
| CX-26 | 按内容识别格式 | `*.csv.xls`（实为文本）能按 CSV 解析；AppleDouble、`._*`、`~$*`、`@syno*`、`.DS_Store` 会被跳过；单个文件失败只记 `files[].error`，其他文件照常导入（附录 A 样本 S10） | U | P0 |
| CX-27 | 表头不在第一行 | 在前约 10 行内找命中别名最多的一行作为表头（附录 A 样本 S2、S4） | U | P0 |
| CX-28 | 一个文件对应多行（多语言翻译表，附录 A 样本 S9） | 按（文件, 字段）合并：非空优先，keywords 取并集并去重；显示时中文优先、没有中文时用英文；结果和导入顺序无关 | U + I | P0 |
| CX-29 | 去扩展名匹配和可选 fuzzy（附录 A 样本 S6、S11、S12） | `Foo` 能匹配 `Foo.wav/.flac`；fuzzy 只作为“建议”显示在预览里，用户确认前不写入 | I + G | P1 |
| CX-30 | **合并列：回滚后逐字节一致** | 导入再回滚后，受影响行的 `bwf_description_tokenized` 和 FTS 内容与导入前逐字节相同（对比 hash），并且等于 recompute(内嵌描述 + 当前仍有效的厂商文本)；被回滚批次的数据从 `vendor_metadata` 删除；`vendor_import_log` 里的 before-image 全部用到；其他批次的数据不受影响 | I | P0 |
| CX-31 | **合并列：重新导入幂等** | 同一份文件导入 N 次（包括回滚后再导入），派生文本里的厂商部分只出现一次，内容与导入一次时逐字节相同；`bwf_description_tokenized` = recompute(内嵌描述 + 当前有效的厂商文本)；`vendor_metadata` 里没有重复行 | I | P0 |
| CX-32 | **合并列：内嵌描述更新时保留厂商文本** | 文件重新扫描、BWF/Vorbis 描述变化后，重算出的 `bwf_description_tokenized` 仍包含当前有效的厂商文本；每一步都满足 `bwf_description_tokenized` = recompute(新的内嵌描述 + 当前有效的厂商文本)；内嵌原文只在原来的字段里，厂商原文只在 `vendor_metadata` 里；之后回滚厂商导入，结果只剩新的内嵌描述。显示时按 用户 > 厂商 > 内嵌 选择，但两种来源都能搜到 | I | P0 |
| CX-33 | 导入或回滚完成后的快照 | snapshot 修订号变化，客户端收到“刷新快照”提示；`source_search_rev` 保持 3 | I | P1 |

## 6. 迁移 schema 8→9

按服务端稿与 #216：迁移建目录段索引 `audio_dir_search` 与 `audio_dir_runs`（计入 `path_build`；设计稿副本约 0.8–1.3 s，含 optimize），主表 `audio_search` 不动、不重建，tokenizer 仍是 t3；NFC 和全角/半角归一化只用于 `audio_dir_search` 及发往它的查询。格式常量 `PLAN_B_V9_FORMAT = "ale-s9-cld1-t3-m2-p1-a1-sd1"`（原逐文件后缀 `sp1` 已停用，现为 `sd1`）。迁移里不做 VACUUM。旧逐文件表 `audio_path_search` 不再建。

| ID | 场景 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| MG-01 | 断电或被 kill -9（分别在目录段回填的批边界和批中间，optimize 阶段，切换事务里） | 重启后从 `v9_checkpoint.last_id` 继续；回填用 delete+insert，所以结果幂等；最终结果和一次跑完一致；在切换前中断时，库仍是可用的 schema 8 | I：在多个点注入 kill；虚拟机强制断电至少 1 次 | P0 |
| MG-02 | 空间预检 | 空余空间 < 库大小 ×1.3 时不开始迁移，`/admin/api/migration` 返回 `INSUFFICIENT_SPACE`，客户端显示空间不足提示；分别测 1.29× 和 1.30× 两个边界 | I：配额卷或 loop 设备 | P0 |
| MG-03 | 自动备份失败 | 中止迁移，不修改原库；回滚备份 `audiolib-rollback-*.db` 通过 integrity/FK 校验后才继续；备份永远不自动删除 | I | P0 |
| MG-04 | DB 文件只读 | 不尝试迁移，以只读方式打开或给出明确提示 | I | P0 |
| MG-05 | DB 被另一个进程锁住，或另一个服务端实例共用同一个 DB | 迁移需要独占 `*.upgrade.lock`；拿不到锁时等待后提示，不损坏数据 | I | P0 |
| MG-06 | 10.6 GB 库副本的迁移耗时 | 记录总耗时、每批耗时的 p50/p95/max、optimize 耗时、切换事务耗时；目录段索引计入 `path_build`（设计稿副本约 0.8–1.3 s，含 optimize；生产副本 path_build 0.81 s）。记录含备份的总耗时（服务端稿预期 <5 min），并确认没有主表重建阶段。迁移报告的阶段键仍为 `backup` / `path_build` / `optimize` / `switch`。旧逐文件表约 70 s 的数字不再作为本索引的基准 | C | P0（必须记录） |
| MG-07 | 回滚到备份 | 切换前取消：DROP 新表和 checkpoint 后回到纯 s8，不需要还原；切换后：用 `audiolib-rollback-*.db` 还原，schema 8 客户端能正常打开，搜索结果和基线一致 | I | P0 |
| MG-08 | 迁移跑两次 | 第二次是空操作 | U | P0 |
| MG-09 | 降级：旧客户端打开 schema 9 库 | 拒绝打开或以只读模式打开，并给出提示，不能写坏库 | I | P0 |
| MG-10 | optimize 后执行 integrity-check | `count(audio_dir_search) = count(folders)`；`audio_dir_runs` 覆盖的文件数 = `sum(n) = count(audio_files)`，孤儿 runs = 0；主 FTS 和迁移前完全相同（行数一致，FTS shadow 表逐字节对比 hash 相同） | U | P0 |
| MG-11 | 自适应批大小 | 每批耗时 ≤2 s（p95 和 max 都记录）；上一批慢了下一批行数会减少；在并发搜索、并发扫描、慢盘三种负载下都满足；写锁最长阻塞时间 ≤2 s | I + C | P0 |
| MG-12 | 迁移里没有 VACUUM | 断言迁移路径上没有执行 VACUUM；迁移后的库大小记录下来；VACUUM 可以作为单独的维护操作执行 | U + C | P0 |
| MG-13 | 运行中磁盘写满（`SQLITE_FULL`） | 迁移转为 paused 并报错，`v9_checkpoint` 保留；腾出空间后 resume 能续跑，结果和一次跑完一致 | I | P0 |
| MG-14 | 管理端点 `/admin/api/migration` 的 `start / pause / resume / cancel` | 状态机为 idle → running ⇄ paused → done/failed；状态和 `completed/total/batch_ms_p50/eta_s` 正确；没有 manage 权限时拒绝；重复 start 不会启动第二个任务 | I | P0 |
| MG-15 | 迁移期间的服务可用性 | 回填期间业务读写照常，旧 s8 快照继续可读；只有切换事务那一刻返回 503 + `Retry-After`；`/health.upgrade` 显示阶段和进度 | I | P0 |
| MG-16 | `AUDIOLIB_DEFER_SCHEMA9=1` | 启动时不自动迁移，库仍是 s8；管理员调用 start 后才开始 | I | P1 |
| MG-17 | 回填期间有新文件扫描进来或目录改名 | 新目录和改名由目录段维护写进 `audio_dir_search` / `audio_dir_runs`；回填完成后不变量与 PS-12 一致 | I | P0 |
| MG-18 | 切换事务写入的内容 | `source_search_rev=3`，features 包含 `search_path`、`vorbis_tags`、`vendor_metadata`，`user_version=9`，在同一个事务里提交 | U | P0 |

## 7. 版本协商与提示

| ID | 场景 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| VN-01 | 旧客户端连新服务器 | 过渡期拿到 s8 兼容快照，能正常同步和搜索（见 7.1）；过渡期结束后收到 409 `SNAPSHOT_FORMAT_UNSUPPORTED`，提示升级客户端，不崩溃 | I | P0 |
| VN-02 | 新客户端连旧服务器（schema 8） | 隐藏路径范围和厂商相关功能，基础搜索正常，不弹提示 | I | P0 |
| VN-03 | 快照 schema 低于 `MIN_SNAPSHOT_SCHEMA_VERSION`，或高于客户端支持的版本 | 给出清晰的错误信息 | U | P0 |
| VN-04 | 服务器 capabilities 字段缺失或未知 | 视为旧服务器 | U | P1 |
| VN-05 | “本地库需要迁移”的提示 | 文案准确；可以“稍后”关闭；同一版本只提示一次 | G | P1 |
| VN-06 | 服务器版本或 schema 与客户端不匹配的提示 | 每个会话只提示一次，不重复弹窗 | G | P1 |
| VN-07 | 客户端有更新可用；离线时检查更新 | 离线时静默失败，不阻塞启动 | G + I | P1 |
| VN-08 | snapshot 格式 ID 和修订号 | 验收目标：完成 s9 迁移后的格式 ID 为 `ale-s9-cld1-t3-m2-p1-a1-sd1`（`PLAN_B_V9_FORMAT`），`source_search_rev=3`、`search_path_rev=1`，三项 feature 同事务发布；主表 tokenizer 为 `t3`，不出现 `t4` 或 `sp1`。目录标记不等于 Vorbis/厂商功能已验收；catalog 及压缩变体的现状差异见下文 | U + I | P0 |
| VN-12 | 目录段版本兼容：只认识 `sd1` 的客户端收到 `sd2` 的目录段数据（包括格式 ID 被篡改、header 和快照内容不一致） | 判定为不兼容：重新拉取它能接受的格式，拿不到时禁用路径搜索（隐藏「仅路径」，「全部」只查主表）并提示升级；**绝不能**用 sd1 规则去查 sd2 的目录段 token（不会静默错配）；主表搜索照常工作，结果不受影响 | U + I | P0 |
| VN-09 | `/capabilities` 的结构 | 验收目标：只新增字段、不改旧字段；按实际库/角色报告 `compatibility.features`、`snapshot_read_formats`、`database_write_formats` 与 `library_schema`、`schema_migration`；只在业务端点可用时报告 `vendor_import` 能力及限额，旧客户端解析不报错。当前实现边界与待验收项见下文，不把设计 JSON 当作已通过响应 | U + I | P0 |
| VN-10 | 迁移中的客户端提示 | 客户端根据 `schema_migration.state`、响应头 `X-AudioLib-Schema-Migration`（`deferred|running|paused|failed` 等形态）和 `/health.upgrade` 显示“服务器正在升级库（x%）”；切换时收到 503，会按 `Retry-After` 重试，不报错退出 | I + G | P1 |
| VN-11 | 迁移完成后的新快照 | 服务端主动预生成 s9 zstd，首个客户端不需要等约 44 s 的生成；`snapshot_id` 变化后客户端提示刷新 | I + C | P1 |
| VN-13 | 搜索分页兼容（承接 VN-01）：仍接受旧 `offset` / `page`；同时传时 `cursor` 优先于 `offset`/`page` | 只传旧 `offset` / `page` 行为不变且与 s8 兼容；同时传时 `cursor` 优先于 `offset`/`page` | I | P0 |
| VN-14 | 不传 `scope` 与显式 `scope=main`；对照 schema 8 同查询 | 两者结果 ID 序列与命中集合完全一致，且等于 schema 8（s8 兼容）；客户端「仅文件名」不传参数 | I | P0 |

**VN-08/09 当前实现与验收边界（2026-11-04，#204–206 扩展候选）：**
完成服务端迁移的 s9 库与普通本地库的目标不同：`SCHEMA_VERSION` / `TARGET_SCHEMA` 仍为 8，
本地升级 UI 当前仅构建路径索引，专用 server 打开入口才允许 s9 可写库。
通用客户端 manifest 列出已知 schema 3–9 完整格式的只读能力，普通写目标仍只列 s8；
服务端 capabilities 的 `database_write_formats` 则按实际源库列 s8 或完成态 s9。
真实读9/写8边界、专用 server9 写入及完整 owned 快照校验已实现，相关回归必须绑定本轮源/二进制身份，
不得仅将全局常量改成 9 或宣称所有本地库已迁移。restore/maintenance 仍保留 s8 写 guard。

`CATALOG_REVISION=2` 已实现；catalog revision 是信息字段，不做相等版本准入，不代替 marker/协议/格式验证。
当前候选的程序版本已冻结为 **0.9.0**，尚未正式发布；升级提示 `min` 取实际构建版本，旧0.8.7夹具与验收记录保留原身份。
`-z1` 是 Metadata 压缩存储格式；快照外层 zstd 传输不构成 s9 `-z1` 实现证明。

VN-09 当前服务端输出 `library_schema.{current,source_search_rev,snapshot_formats_served}`、
`schema_migration.{state,endpoint}`、真实 `vendor_import.{endpoint,revision,max_file_bytes,max_rows,max_row_text_bytes}`、
`client_upgrade` 与四个 `search_scopes`。导入限额为每文件 52,428,800 bytes、200,000 行、每行 262,144 bytes；
路由仍受鉴权、manage 权限、库身份和实际格式准入约束。
`GET /library` 的 `schema_version`/format/stats 是协商结果，新增 `library_schema` 是实际源 schema；
`library_id`/revision 绑定源库。现代端同样发送完整 accept/max 头；历史无标记 `/library` 可默认看兼容8信息，
不构成无标记快照下载的例外。能力协商须同时验证这些角色，不能只看程序版本字符串。
缺字段时客户端回退，未实现入口隐藏；字段兼容与格式兼容分别验收。
本轮具体测试/真实旧客户端配对结果另记，不将新增字段或旧身份报告当成本轮验收已通过。

### 7.1 s8 兼容快照（设计目标：过渡期暂定一个版本周期）

当前候选已实现 s9→s8 派生及协商发放，默认 `legacy_snapshot_enabled=true`，计划覆盖 0.9 过渡周期；
以下 **SC-01..10 仍是验收断言，不因实现存在而自动通过**。真实官方 0.8.7、原生新客户端、失败缓存保全及
大库成本均须按本轮候选身份分别验证，不能以早期“旧端拒绝9”的报告代替过渡成功验收。

兼容副本只在 owned staging 修改：按 WAV 过滤文件和已知关联，移除目录索引/capture、正式 vendor 表，
不重建主表或主 FTS；WAV 原有 rev3 词项、Metadata 扩展镜像和用户标注保留，统计按过滤后行刷新。
原生/兼容及 plain/zstd 缓存和任务隔离，使用同一源 library_id/revision；新 revision 重新派生最新副本。
不安全的未知扩展关联、校验失败、取消或空间不足均不得发布兼容副本，且不影响有效原生表示。
无标记 `/library` 的 WAV 聚合不先做 GB 级复制；快照下载仍要求 catalog、精确 accept 格式和修订头。
兼容信息/快照共享 recommended 提示，原生9不误提示；关闭过渡后旧端 required/409，保留最后已验证缓存供离线读取。
旧端即使可读派生8，也不能写源9。SC-03 的 `vendor_metadata` 指正式存储表，非保留在 WAV JSON 中的未知扩展镜像。

| ID | 场景 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| SC-01 | 旧客户端（v0.8.7）连 s9 服务端 | 只声明接受 s8 时，拿到由 s9 派生的 s8 兼容快照；同步成功，基础搜索正常，不崩溃 | I：真实旧版二进制 | P0 |
| SC-02 | 过滤非 wav 行 | s8 快照里没有 flac/ogg 行；行数等于 s9 中 wav 行数；对 wav 行，EM-06 的命中数和 ID 集合与 schema 8 基线一致 | C：对比脚本 | P0 |
| SC-03 | 派生快照的结构 | 没有 `audio_dir_search`、`audio_dir_runs` 和 `vendor_metadata`；`source_search_rev=2`；`user_version=8`；格式 ID 是 `ale-s8-cld1-t3-m2-p1-a1`；主表直接沿用 s9 的主表（不重建）；在 s8 快照上用 v0.8.7 跑 EM-06 和 PG-01 查询集，结果和 schema 8 基线一致；library_id 和 s9 相同；文件夹统计和过滤后的行一致 | U + I | P0 |
| SC-04 | 升级提示 | 拿到 s8 快照的客户端收到响应头 `X-AudioLib-Upgrade-Available`（取值同 `/capabilities.client_upgrade.level`），`/capabilities.client_upgrade` 里也有提示；客户端显示非阻塞的升级提示，每个会话只显示一次 | I + G | P0 |
| SC-05 | 快照发放规则 | `x-audiolib-accept-snapshot-formats` 包含 s9 的客户端总是拿到 s9，不会退回 s8；只接受 s8 的客户端才拿到 s8 | I | P0 |
| SC-06 | 写操作只接受 s9 客户端 | 旧客户端（s8）发写请求返回 409，库不被修改；s9 客户端的写请求正常 | I | P0 |
| SC-07 | 派生快照里的厂商/Vorbis 文本 | `bwf_description_tokenized` 里已经包含 rev3 的厂商和 Vorbis 文本；旧客户端能读、能搜到，不报错 | I | P1 |
| SC-08 | 生成成本与失败处理 | 记录生成耗时（预期 1–2 min，没有主表重建）和多占的磁盘（约 10.6 GB plain + 约 959 MB zstd）；生成失败不影响 s9 快照的发布；生成前做空间检查 | C + I | P1 |
| SC-09 | 厂商导入或扫描后 | s9 修订号变化后，s8 兼容快照也重新派生，旧客户端能拿到更新 | I | P1 |
| SC-10 | 过渡期结束（关闭 s8 快照） | 旧客户端收到 409 `SNAPSHOT_FORMAT_UNSUPPORTED` 和“需要升级客户端”提示；本机已有的旧快照仍能离线浏览 | I + G | P1 |

## 8. 本地模式与服务器模式一致性

| ID | 场景 | 预期 | 验证 | 优先级 |
|---|---|---|---|---|
| PA-01 | 同一个库，本地模式和服务器模式（remote_mode.rs:114/156）跑 PG-01 的查询集 | 结果 ID 集合与排序一致 | C | P0 |
| PA-02 | 文件夹过滤加路径搜索 | 两种模式一致 | I | P0 |
| PA-03 | 服务器端路径和客户端看到的路径前缀不同 | 远程路径映射后，路径 token 仍然一致 | I | P1 |

## 9. 合入 beta 前的回归冒烟清单

1. PG-01 到 PG-16 门禁全部通过（含 `hdd_A_Lib explosion` 等 PG-16 过闸例，以及 #216 产品代码里的目录空短路）（延迟门禁：每条查询 5 轮 p95 的中位数 ≤ schema 8 × 1.2，Paul 2026-11-03 决定；重点是 PG-10/11「全部」范围的热、冷 p95，PG-12/13/14 正确性，以及 PG-07 ≤15%），数据附在 PR 中。
2. EM-06 的 4 个基线命中数和 ID 集合对比通过；EM-09 全角/NFD 样本主表命中数一致、路径搜索能匹配所有变体。
3. 在 10.6 GB 库副本上完成一次 8→9 迁移，包括 MG-01 断电/kill 续跑、MG-02 空间预检、MG-11 单批 ≤2 s、MG-13 `SQLITE_FULL` 暂停后续跑、MG-07 回滚。
4. 本地/服务器一致性 PA-01 通过。
5. VN-01、VN-02 新旧版本交叉测试通过；SC-01、SC-02、SC-03、SC-04 和 VN-12 用真实 v0.8.7 客户端验证。
6. 导入一份接近上限的样本（csv、xlsx、xls 各一份），然后回滚（CX-10、CX-16、CX-19～CX-22、CX-30～CX-32）。
7. `cargo test --workspace` 全绿；在 GUI 中手动搜索中文、路径片段，切换「全部 / 仅文件名 / 仅路径」，并验证防抖与取消。

## 10. 设计决定（#209 @ cddc7e3、服务端稿及之后的决定）

1. **路径**：目录段索引 `audio_dir_search` + `audio_dir_runs`（#216），主表不变，路径不参与主表 bm25。`scope` 省略或 `main` 时顺序与 schema 8 一致；`all` 按三层排序，多词按词计算（主 ∪ 目录段）再求交集；`path` 严格要求每个词均命中目录，不用主字段补足。路径词估计命中超过全库一半时丢弃该词的目录集。加静态停用词表；根名默认不入索引。目录命中集为空且主表 `LIMIT 1` 也空时返回空页并跳过全量主扫描。→ §1、§2
2. **中文分词**：用 jieba 词典分词，不用 bigram。→ PS-03
3. **Vorbis**：规范字段按映射白名单；全部保留的 textual key 原文存 JSON 并参与 Metadata 搜索，图片键除外，受提取预算约束。→ §3
4. **厂商文本**：并入 `bwf_description_tokenized`，不新增 FTS 列，所以没法单独设权重；原始数据在 `vendor_metadata`；显示优先级 用户 > 厂商 > 内嵌，两种来源都能搜到。→ PG-04、CX-30～CX-32
5. **匹配规则**：一行对应多个文件时标为歧义、不自动写入；一个文件对应多行时非空优先、keywords 取并集；显示时中文优先，没有中文时用英文。→ CX-11、CX-28
6. **旧客户端**：过渡期发 s8 兼容快照（过滤非 wav 行，附带升级提示头），之后硬拒绝并提示升级。→ §7.1
7. **索引增长 ≤15%**：作为门禁（方案 B 实测 +14.2%）。→ PG-07
8. **tokenizer 保持 t3（推翻之前的 t4 决定）**：主表 `audio_search` 不做归一化、不重建，s9 格式为 `ale-s9-cld1-t3-m2-p1-a1-sd1`（`PLAN_B_V9_FORMAT`）。原逐文件后缀 `sp1` 已停用，目录段版本是 `sd1`；NFC 和全角/半角归一化只用于 `audio_dir_search` 及发往它的查询，以后再改就升到 `sd2`。迁移建 `audio_dir_search` 与 `audio_dir_runs`；s8 兼容快照按原计划从 s9 派生。→ EM-09、§6、VN-08、VN-12、SC-03
9. **导入上限**：50 MB（52428800）/ 20 万行 / 单行 256 KiB。→ CX-19～CX-23
10. **s8 兼容快照保留一个版本周期**（等 Paul 最终确认）；显示语言中文优先，没有中文时用英文。→ §7.1、CX-28

## 附录 A. 真实厂商元数据表格普查与解析基准（2026-11-03）

只读采集，没有修改素材库。素材根目录记为 `<A_Lib>`（NAS 共享，下文路径都相对于它），服务器镜像库 `library-r824` 中共 909,342 条 `audio_files`，有 38 个一级厂商目录。文件本身受授权限制、体积也大，所以没有入库，这里只记录特征。

### A.1 普查结果

- 按名称匹配（`*.csv/*.tsv/*.xls/*.xlsx`，或文件名含 metadata/soundminer）共找到 610 个文件，合计 182 MB。其中表格类 449 个：`.xls` 209、`.csv` 127、`.xlsx` 103、`.txt`（Soundminer 导出）10。另外还有 PDF 125、PNG/JPG 30、`.numbers` 1，以及 Synology 的 `@synoeastream/@synoresource` 旁路文件。
- 文本编码（137 个 csv/txt）：UTF-8 117、ASCII 7、UTF-8 BOM 3、**UTF-16LE BOM 10**（9 个 Soundminer `.txt`，加上 1 个扩展名为 `.csv` 的 Tab 分隔文件）。**没有发现 GBK 或 cp1252**，也没有解码错误。
- 分隔符：`,` 122、`\t` 11、`;` 4。
- 工作簿：多 sheet 的有 57 个（最多 12 个 sheet）；有 15 个文件的行宽不一致（ragged）。
- 解析失败 5 个：2 个 Sonniss `*.csv.xls` 实际是纯文本 CSV；2 个 SoundMorph `*.xls` 实际是 AppleDouble 资源叉（魔数 `00 05 16 07`，内容 "Mac OS X"）；1 个 `~$*.xlsx` 是 Office 锁文件（0 KB）。
- 常见表头：Filename 366、Keywords 256、Description 255、Category 221、Library 211、Manufacturer 199、Source 196、BWDescription 173、SubCategory 162，也有 CatID、FXName、RecID、TrackTitle 等。很多文件第 1 行是标题或厂商名，真正的表头在第 2～3 行。

### A.2 代表性样本

| # | 路径（相对 A_Lib） | 特征 |
|---|---|---|
| S1 | `1_Boom/Crowds Concert Audiences/00_Crowds_Concert_Audiences_Metadata.xls` | BIFF xls，3.4 MB，4692 行，Soundminer 列齐全，是全量中 RSS 最高的样本 |
| S2 | `1_Pro Sound Effect/The Odyssey Collection/Designed/The-Odyssey-Collection-Designed-Metadata.xlsx` | 行数最多（5159），第 1 行是标题，表头不在首行 |
| S3 | `1_Boom/Cannons/Construction Kit/00_Cannons_CK_Metadata.xlsx` | 12 个 sheet，ragged，解析最慢之一 |
| S4 | `3_VO Lib/人声_YDH配音源文件/《女神竞技场》配音格式需求_4.2.xlsx` | 体积最大（12 MB，主要是嵌入图片），但只有 905 行，8 个 sheet，中文，没有 Filename 列，只能按不带扩展名的 stem 匹配 |
| S5 | `1_Sound Ideas/Premier/Premiere_Edition_6_The_2448/.../PE6 METADATA.txt` | Soundminer 导出，UTF-16LE BOM + Tab，扩展名是 `.txt` |
| S6 | 同目录 `PE Vol. 6 Catalog.xls` | Filename 列不带扩展名；一份清单覆盖多张盘（`1of2`/`2of2` 目录） |
| S7 | `1_SoundBits/Whooshes Impacts 2/Whooshes And Impacts 2.csv` | 扩展名是 `.csv`，实际是 UTF-16LE + Tab；只有 RecID，没有 Filename 列 |
| S8 | `2_Cartoon Lib/Vintage_Anime_SFX/Vintage Anime SFX/VASE.csv` | `;` 分隔，表头首列为空 |
| S9 | `4_Vehicle/Car Collection/Volvo_XC90/_Documents/Metadata Translations/Volvo XC 90 Sound File List - 中国語 (Japanese).csv` | UTF-8 CJK，同一库有 6 种语言的翻译表（会重复导入） |
| S10 | `4_Others/Sonniss.com 2016 FX/Audio.Bundle.Part2/The Complete Sonniss Catalog - 2016.csv.xls`；`1_SoundMorph/Mechanism/_SoundMorph_Mechanism_Soundminer.xls`；`1_Boom/Aquatic Predators/~$00_...xlsx` | 伪装 xls（实为 CSV）、AppleDouble、Office 锁文件 |
| S11 | `1_The Recordist/TR_Guns/TR_Guns_Rifle/The_AK_HD_Pro_Pt1/.../The_Recordist_The_AK_HD_Pro_List_XLS.xls` | 没有 Filename 列，只有 Description（`GUN: Assault Rifle: ... - DPA4062`），而实际文件名是去掉标点的近似值 |
| S12 | `1_Pro Sound Effect/PSE_Gen Collection/Doors/Doors-Metadata.xlsx` | Filename 后缀 `_PSE_DRS_096` 和磁盘上的 `_PSE_GEN3_jif2O` 不一致（重新打包时改了名） |

### A.3 解析基准

环境：Windows 开发机，Rust release 版，依赖 `csv 1.4` + `encoding_rs 0.8.44` + `calamine 0.38`。先把文件复制到本地 SSD 再解析，这样不计 SMB I/O。实现是一次性读入，并把全部单元格物化为 `Vec<Vec<String>>`。parse 取 5 次的中位数；峰值 RSS 取进程 PeakWorkingSet 的最大值，包含约 5.4 MB 的进程基线。

| 样本 | 大小 | 行（含表头） | parse ms | 进程 wall ms | 峰值 RSS MB | 行/秒 | 行命中率 | 文件覆盖率 |
|---|---|---|---|---|---|---|---|---|
| S1 Crowds .xls | 3478 KB | 4692 | 21.2 | 29.9 | 29.7 | 221k | 100% | 100% |
| S2 Odyssey .xlsx | 310 KB | 5159 | 11.4 | 18.9 | 10.3 | 454k | 99.9% | 95.6% |
| S3 Cannons CK .xlsx | 1773 KB | 3370 | 29.9 | 37.8 | 9.6 | 113k | 27.1% | 99.8% |
| S4 女神竞技场 .xlsx | 12114 KB | 905 | 3.2 | 10.1 | 5.9 | 287k | 59.2%（仅 stem） | 0%（精确） |
| S5 PE6 .txt UTF-16 | 1008 KB | 1100 | 2.1 | 9.4 | 9.2 | 535k | 31.8% | 100% |
| S6 PE6 Catalog .xls | 282 KB | 1100 | 1.9 | 8.8 | 7.0 | 589k | 31.8%（仅 stem） | 0%（精确） |
| S7 Whooshes .csv UTF-16 | 81 KB | 267 | 0.3 | 7.1 | 5.4 | 810k | 0% | 0% |
| S8 VASE.csv `;` | 61 KB | 289 | 0.3 | 7.1 | 5.5 | 928k | 100% | 100% |
| S9 Volvo JP .csv | 657 KB | 856 | 1.4 | 8.3 | 6.5 | 594k | 100% | 92.6% |
| S10 Sonniss/AppleDouble | 109/4 KB | 0 | 0.1 | 6.7 | 5.4 | – | 解析失败 | – |
| S11 Recordist AK | 60 KB | 227 | 0.4 | 7.1 | 5.6 | 632k | 0% | 0% |
| S12 PSE Doors | 16 KB | 126 | 0.7 | 7.5 | 5.8 | 182k | 0% | 0% |
| 合成 200k 行 CSV（10 列） | 39 MB | 200001 | 117.6 | 171.8 | 161.6 | 1.70M | – | – |
| 合成 200k 行 XLSX（10 列） | 7.6 MB | 200001 | 788.5 | 845.4 | 248.9 | 254k | – | – |

全量 449 个文件：共 230,345 行，parse 累计 797 ms，进程 wall 累计 4.2 s，峰值 RSS 最大 29.7 MB、中位数 5.8 MB。遍历厂商目录（SMB）收集文件名，单个目录最慢 1.16 s。匹配规则：任意单元格的 basename（不区分大小写）等于磁盘上的文件名记为精确命中，等于 stem 记为 stem 命中；匹配范围是表格所在的厂商目录，会跳过 Info/Data/Docs/Catalog/Metadata/Translations 这类子目录。在 440 个有音频的目录中，精确文件覆盖率 ≥90% 的有 362 个，为 0 的有 33 个；只能按 stem 命中的文件有 21 个。

### A.4 对设计（#209）的影响

1. **编码**：没有遇到 GBK 或 cp1252，但**必须支持 UTF-16LE BOM**（Soundminer 导出，且可能用 `.csv` 扩展名）。检测顺序建议为 BOM → UTF-8 校验 → GBK 兜底。**分隔符必须嗅探**（`,` `;` `\t`），不能由扩展名决定。
2. **按内容识别格式**：`.xls` 可能是文本 CSV 或 AppleDouble 资源叉。需要按魔数分派（OLE `D0CF11E0` / ZIP `PK` / 文本）；应跳过 `._*`、`~$*`、`@syno*`，并且容错不中断。
3. **表头定位**：大量文件第 1 行是标题。需要在前 N（≈10）行里扫描含 `Filename`/`File Name`/`FileName` 的行作为表头，并逐 sheet 处理（多 sheet 占 13%）。
4. **匹配键**：
   - 精确 basename（不区分大小写）在约 82% 的目录可行。
   - 必须加一级 **stem（去扩展名）匹配**（Sound Ideas Catalog、VO 表格）。
   - 一份表会覆盖多个分卷或 Part 目录（Recordist、Sound Ideas、Boom 多 sheet），所以**匹配范围应是整个厂商根目录或整库的 basename 索引**，不能只看同级目录：行命中率低，但文件覆盖率接近 100%。
   - 约 7% 的表格没有 Filename 列（只有 Description 或 RecID），或者文件名在重新打包后被改过（PSE），这些无法可靠匹配。建议在导入预览里标为"未匹配"，不做模糊匹配，或者把模糊匹配作为可选项。
   - 多语言翻译表会对同一文件重复导入，需要定义合并或去重规则（以 path + 列名为键，或按语言区分）。
5. **内存**：真实表格最大只有 5159 行，峰值 RSS 不超过 30 MB，不构成风险。合成的 200k 行 XLSX 全量物化后达到 249 MB、0.79 s，CSV 为 162 MB；这些数字按 `Vec<Vec<String>>` 物化计，流式写库会更低，但流式方案未实测。calamine 对 xlsx 必须整 sheet 读入 Range，所以建议给导入设置行数和文件大小上限（例如 xlsx 50 MB），或者分 sheet 处理并分批写入。体积大不等于行数多：12 MB 的 xlsx 主要是图片。

基准工具放在开发机的临时目录，不在仓库内：`rbench/` 是 Rust 程序，`results.jsonl` 是全量结果，`samples_bench.json` 是样本结果。
