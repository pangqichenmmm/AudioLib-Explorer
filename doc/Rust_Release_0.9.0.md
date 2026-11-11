# 0.9.0 发布准备与两端迁移方案

状态：**发布准备中。#204–206含手动及同目录自动表格导入，源级、HTTP、真实表格及额外实际音频验收已有通过证据。Windows客户端 `f70156e` 开发候选已构建、验收并完成固定安装，本轮核心GUI补验因工具阻塞未完成，保留release门禁；`fe13b05` 双二进制服务端开发包及既有选定GUI证据保留原身份。完整五轮PG05仍未达到严格1.1门槛，未发布。**
阶段调整：当前三项核心功能及Waveform的受影响定向检查已通过，保留必要数据保全，开发候选已交付；PG05、百万记录性能、全平台验收及全局回归留到正式release阶段，现有失败/未验状态保留，不作为开发功能先交付的前置通过条件。
本次版本冻结将 Cargo、锁文件及当前候选说明统一为 `0.9.0`，相对已发布 `v0.8.7` 为 MINOR 升版；最终 main/tag 身份与跨平台 payload 验收仍待完成。本阶段不创建标签或发布。
生产服务、工作库和源 WAV 未变更；主工作区固定 `local_release/` 已更新为 `f70156e` 客户端开发包并核对907个文件、更新9个文件，包含服务端目标常量和Waveform共享连接修正。既有fe13包及GUI结果不回标为当前身份。
feat → beta、beta → main、标签、发布以及实际服务部署各须取得相应授权。

## 1. 候选范围与证据身份

本次搜索范围扩充的核心范围包括路径可搜、Vorbis 标签提取、厂商 CSV/XLS/XLSX Metadata 导入并参与搜索。
这三项分别对应原始任务 #204、#205、#206；2026-11-04 用户要求再次核对并将不一致项列入 fix。
用户随后明确要求完成三项任务的全部开发。实现现已包含四种搜索范围与筛选/排序、真实 FLAC/Ogg 标签提取，
以及本地和服务端 CSV/XLS/XLSX 手动导入；本轮端到端、资源和交付验收仍进行中。
用户追加要求：普通 WAV 扫描同时自动关联同目录厂商表格，与内嵌 Metadata 比对、去重导入。
该自动流程已实现并列入 #206 的本次发布范围，Windows新增/受影响99项定向回归及真实表格验收通过；另有原生Mac62项及后续独立60项源检查、实际NAS109条音频和fe13选定GUI/包交付证据，范围分别见1.3、1.4节。
迁移管理、客户端提示/刷新、Default v2 和播放器反馈为配套范围。
完成全部核心功能并验收后才能冻结完整发布范围；schema feature 标记、客户端适配或原型不能代替交付。
原始设计与验收目标见 [服务端设计](Server_Search_Metadata_V2_Design.md)、
[测试计划](TEST-SEARCH-METADATA-V2.md) 和 [交接记录](HANDOFF-CODEX-20261103.md)。

| 输入 | 身份与实际状态 |
| --- | --- |
| 已发布基线 | `v0.8.7`，应用源码 `b5a98ff98fed501aafff0f6a4ef5015c8b4d435d`；历史七包及固定安装记录见 [0.8.7](Rust_Release_0.8.7.md) |
| Search Metadata v2 集成基线 | `feat/search-metadata-v2` 的 `f252620`；包含目录搜索、scope、升级提示及 EM/MG harness |
| 后台服务端任务 | 本地 `d29cd35f215d4aa78a583de09cf57467d31985c5`，第五轮完成；检查时远端 #225 仍为 `47f0205` draft，不能把本地完成视作 PR 已合入 |
| 后台客户端任务 | `feat/search-v2-client-ux2` 原 `1988eef` 合入 `f252620` 后完成，37 文件含合并输入，交付时保留未提交 merge；其最终 diff 已收入候选 `9a13978` |
| beta UI 输入 | `origin/beta` 的 `cfe0118`，含 `b5d7552` 的 Default v2/播放器反馈及 `131bbd6` 的 ignore 更新 |
| 首个合并候选 | `codex/release-090-prepare` 的 `02036a6`，整合上述服务端、客户端和 beta UI；这不是已验收或已发布版本 |
| 较早兼容修正 | `c2d2f8a`：完成 s9 快照读入、owned 校验、增量 Metadata、服务端 relink、本地格式标记和目录旧写入捕获；当时隐藏尚未实现的厂商导入入口 |
| 上一轮已验收/安装源码 | `bebf27211af741f1b3f11b7f2190b60e791fbe08`；当时尚无完整 Vorbis/厂商导入链路，下文历史结果只覆盖当时范围 |
| 本轮实现基线 | `4e77492aa78525fbed1f8c772013ba80a18447f6`，包含 #204–206 实现及新增回归；其后边界修正分别复验，下文记录组合证据，不宣称一次全仓全绿 |
| 迁移与旧 CLI 配对基线 | `d116dcd890fc03c7506c5b514d1338d296e18e68`；真实官方0.8.7 CLI 双快照、旧写拒绝、刷新和离线配对通过；不是最终包身份 |
| 最近完整 MG 身份 | `3562eead704357dcbe6e78bfabc9f083174990c4`，完整MG24/24、41.55秒通过；较早 `fd2e04e` 的40.28秒结果保留，不倒改身份 |
| 最近已提交性能实现 | `0b3f2f7b1de32cf00fe3c4ca46045169b778bbc1`，整页缓存原型；此前 `ffcfbce` 合并 scoped membership/减少重复 Metadata 投影；组合源检查与实际诊断见第5节 |
| 最新已验证改动 | `2c456979872e47cc15f6f52dbe832ad3ed0dd770`：池化升级锁句柄、读取快路径和重复排名维护削减；fmt、DB strict Clippy、35项定向单元、14个DB集成目标123项及2个HTTP目标7项通过；并发诊断仍未达标，不能称发行冻结 |
| 旧GUI和MG配对身份 | `3562eead704357dcbe6e78bfabc9f083174990c4`，包含固定SQL prepared statement复用；该身份已有原生MG、官方旧GUI和下述性能诊断，仍不是发行冻结 |
| 较早COW验证候选 | 基于3562eea：整个AudioFile共享不可变Data、写时复制；五包ASIO strict通过，扫描完成事务的缓存维护遗漏已修复，原Vorbis断言通过。后续44条测试目标调用1225通过/0失败/165忽略，与此前通过范围有重叠；保留其实际输入身份 |
| 后续缓存键优化 | COW及扫描收尾已提交为`20c4024bd3cf31c708f31b9e10f21b5f43e667a8`；其后使用2KiB内联精确序列化查找键，仅在准入时取得所有权；DB strict与61项定向测试通过。容量、代次、revision证明与JSON身份不变，新性能验收待完成 |
| 较早性能修正基线 | `0628cc4a41b54a5d0b2359fcfae34fedf5b44041`；八组完整集合及续页oracle通过，100轮含facets的P95为209.0956ms。完整五轮PG05失败：倍率1.353–1.443；保留原报告，不改写为后续身份 |
| 隐式缓存读取修正 | `9102197`，仅缓存查找以单条真实audio_files读取建立隐式snapshot；语句reset后比较observer前后revision，保留OS lease、reader gate、取消和完整失效契约。fmt、DB test-hooks strict及11条目标调用96项通过/0失败；含6项新取消/脏连接/真实提交交错回归。run68174015单轮warm含facets P95=257.419ms；五组并发比值1.11–1.167，仍无正式PG05通过证据 |
| 成对自动更新修正 | `8f4766e`，当前server布局要求匹配平台迁移器；Unix TAR迁移器执行位与server一致。updater strict、33通过/2忽略，server NoDefault updates3项通过；完整pair安装/缺件拒绝/第二文件失败恢复已有真实文件回归。Unix执行位原生测试和部署脚本验收另行记录 |
| 原生GUI发现的修正 | `9f9b7f1`：Vorbis合法负起始时间及空primer不再造成加载失败，seek与顺序PCM对齐；重预览按文件/sheet身份保留选择。Audio112项及后续golden1项组成113通过/3忽略的组合证据，Desktop vendor15项通过；实际fe13 GUI已复验格式复用及P2所选映射，见1.4节 |
| 同目录自动表格扫描 | 功能整合为 `fe13b05f43cc75ae3de6373aedc390d4afdf2858`：客户端/CLI/服务端扫描CSV/TSV/XLS/XLSX，限定同目录且本轮成功扫描的唯一WAV；原始来源、规范去重、重扫、部分关联和取消保全。Windows99项与Mac62项分别记录，不合计为一次workspace运行 |
| 版本冻结前已验收Windows客户端开发包 | `f70156e0a54dddb812584d24564f2601fe6e4ebe`；运行版本0.8.7，build/package/portable-accept/install通过，主工作区907文件固定安装、更新9个文件及隔离启动通过。本轮核心GUI补验因工具阻塞未完成，保留release门禁；不是最终0.9.0/main/tag产物。fe13服务端包、选定GUI和实际NAS109条保留原身份 |
| 最近完整性能验收 | `fe13b05`，run `81110616bd274a5fa9a07e58483887da`：完整oracle与续页通过、warm100+facets P95=209.4968ms；正式五轮均有充分真实并发，但五组倍率1.19118–1.21311，严格PG05失败；两个首次search+facets合计也超过500ms |
| fe13后源码修正 | `SERVER_TARGET_SCHEMA=9`区分两类写目标，DB快照读上限引用Core；首版`4ec`的Windows27项属于旧输入，`04c`尾部修正28项已在Mac和Windows通过。All窄移植/测试加强的`d1c746fd`保留严格Path/v3；Mac60项和Windows62项核心定向检查各自通过，范围不同不相加。正式PG05留release，f701客户端开发包已交付，最终发行payload仍待验 |
| 待闭环上游输入 | `codex/fix-path-search-timeout` 基于 `8b53f7b33181282893c5424fb027b88874213936`（上游#225）；首轮`ea937cb`完成后已选取All算法及管理提示窄移植，未带入其混合Path/旧计数契约。新一轮CLI仍活跃，最终稳定输入待再次比较，不整合WIP；原f252620身份保持历史事实 |

### 1.1 三项核心功能核对与 fix 任务

下列复选框表示**完整发布验收及交付**，不把源码存在、单项通过或开发诊断当作全部完成。
开发收口复用1.3、1.4节实际NAS、GUI及已有定向证据；核心/Waveform修正已完成相应定向检查及f701客户端包/安装验收，核心GUI补验因工具阻塞未完成，保留release门禁，不重复无关全局或百万性能运行。
原始任务继续使用已有 Issue；已有结果保留实际源码、特性、环境和二进制身份。

| 核心任务 | 已实现并有相应源级/链路证据 | 剩余发布验收 |
| --- | --- | --- |
| CORE-01 / [#204 路径可搜](https://github.com/Freedragonlancer/AudioLib-Explorer/issues/204) | 四种范围、严格目录多词/祖先目录、跨来源AND、筛选/排序/分页及facets；fe13 GUI实际Path/Main和同步链路见1.4 | 严格PG05、首次组合延迟、最终main/tag实际包及匹配平台；不把选定GUI观察扩成全部交互通过 |
| CORE-02 / [#205 Vorbis 标签提取](https://github.com/Freedragonlancer/AudioLib-Explorer/issues/205) | 真实FLAC/Ogg/OGA提取、FTS5、重提取保全、HTTP/离线；fe13实际GUI连续FLAC→OGA→OGG复用输出、Waveform与Ended0.080通过 | 最终版本实际包和平台验收；没有物理输出、ASIO硬件或点击到出声延迟声明 |
| CORE-03 / [#206 厂商表格导入](https://github.com/Freedragonlancer/AudioLib-Explorer/issues/206) | 手动映射/模板/候选和来源回滚；同目录自动扫描、嵌入比对、幂等与取消保全及s8/s9真实HTTP；fe13选定GUI、Windows包/安装及NAS109条通过 | 真实导入并发PG05仍是正式发布门禁；GUI回滚及off-role权限界面未执行，正式HTTP/包证明另列；最终发行平台及身份待冻结 |

具体功能边界：

- **#204：** `main` 是主字段（含 tags、notes、BWF/iXML 和新增来源），`filename` 严格限文件名及其分词列；
  `path` 每个词都须在目录/祖先目录命中，`all` 才允许每词跨主字段与目录组合。
  完整主字段层按 BM25/ID，混合/纯路径层按 ID；支持目录、专辑、收藏、Metadata 条件、允许的其他排序、
  精确/下界总数和不混代的续页游标。深层祖先、稀疏/NULL 成员与 facets 有参考集合回归。
- **#205：** 默认扫描 `wav/flac/ogg/oga`，真实格式 probe、FLAC/Vorbis 解码与 seek 已接通。
  Vorbis 原文保存在 `wav_metadata.vorbis`，重复值、中文、未知文本键保留；规范映射及搜索投影独立，
  损坏/截断/无效编码受预算和警告约束，图片/base64 不进 FTS5；提取版本触发增量重读并保留用户及厂商数据。
  Ogg Vorbis、Ogg FLAC 和链式 Ogg 标签取第一条逻辑流；Opus 仅标签 probe，不默认扫描 `.opus`，不承诺 Opus 播放。
- **#206：** CSV/XLS/XLSX 魔数解析、UTF-8 BOM/UTF-16/GB18030、分隔符/RFC4180、多 sheet/表头、
  列映射与模板、相对路径/vendor root/文件名/stem、可选 fuzzy 建议及歧义显式确认均已接通；全部候选可分页查看。
  单文件上限50MiB、20万行、单行文本256KiB；批次最多512文件、累计输入512MiB，另有实际 ZIP 展开安全预算。
  预览在独立 owned 存储中，不改业务库；提交幂等，取消/中断保留状态和来源；完整 before-image 校验、
  语言优先、当前内嵌内容与其他批次保全均有回归。显示按用户 > 厂商 > 内嵌，各来源仍能搜索；
  完成导入/回滚后修订变化并提示刷新快照。权限或服务器能力不支持时隐藏相应入口。验收范围为 CX-01..33。
  普通扫描还自动处理同目录表格，限定当前扫描确认的唯一WAV，不做全库/stem/fuzzy回退。
  完整安全匹配后才替换该来源的旧贡献；存在未匹配/歧义时合并新值并保留该表全部旧贡献，
  防止插行、重排和部分文件离线时误删。表格缺失/损坏/无数据保留最后有效值，仍可显式回滚该来源。

- [ ] **CORE-01：** 收口最新性能 WIP，满足绝对延迟及 PG05，完成最终 GUI/包的范围、筛选、排序与分页验收。
- [ ] **CORE-02：** 用最终实际包验收格式扫描/提取/搜索/重扫/同步/离线及受支持播放，保持 VB-01..09 和 codec 边界。
- [ ] **CORE-03：** 用最终实际包验收导入/确认/状态/恢复/刷新/回滚和并发性能，保持 CX-01..33 及数据保全断言。
- [ ] **CORE-03-AUTO：** 完成本地与服务端 WAV 扫描中的同目录 CSV/TSV/XLS/XLSX 自动关联，
  校验内嵌/表格去重、重复扫描、仅表格变更、同名隔离、取消恢复及 s8/s9 同步/离线；
  追加 AS-01..08 验收，保留用户标注与其他手工/自动来源，不以手动导入通过替代。

### 1.2 已登记的不一致修正

- [ ] **FIX-01 · P0：范围名称与实际集合一致。** `main` 保留原有 tags、notes、BWF/iXML 等主字段，
  UI 改称“主字段”；新增 `filename` 仅匹配文件名及其分词列。集合回归已通过，最终原生交互待验。
- [ ] **FIX-02 · P0：严格路径多词语义。** `path` 每个词都必须在目录/祖先目录命中；
  目录 `Door` + 文件 `slam.wav` 的 `door slam` 仅由 `all` 跨来源组合命中。参考集合/oracle 已通过，最终包待验。
- [ ] **FIX-03 · P0：路径与筛选/排序组合。** 已接通目录、专辑、收藏及 Metadata 条件，
  `all` 不再因条件退回主字段；参数化 SQL、facets、排序及游标使用一致集合。组合回归已通过，PG05 仍阻塞。
- [ ] **FIX-04 · P1：所有主题的迁移入口。** Studio、Default v2 已接入迁移面板，
  按权限/模式限制动作；源级状态/关闭排空回归已通过，最终包实际打开和操作仍待验。
- [ ] **FIX-05 · P0：排序口径。** 与服务端设计统一：完整主字段层按 bm25/ID；混合和纯路径层按 ID。
  PS-23 已同步，源测试及完整 oracle 保留 `main` 旧顺序并验证混合层参考序列；最终包和性能门槛另验。
- [ ] **FIX-06 · P0：两端迁移和管理边界。** 普通本地库保持 s8 写目标，支持本地目录索引准备、
  新格式扫描和厂商导入；UI 不称其为完整 s9 迁移。显式服务端迁移发布 s9，由专用 writer 写入；
  客户端只读 s8/s9 快照。restore/maintenance 仍受 s8 guard 保护，不能向普通 writer 交入 s9。
  本地新功能验收与服务端迁移分别记录；不承诺旧本地写程序对新增来源投影的保真。
- [ ] **FIX-07 · P0：能力与实际业务。** catalog 已升为2，按角色和实际源库格式返回迁移、
  写入、快照、四种范围及厂商导入能力/限额；真实 Vorbis/厂商生产投影已接通。
  VN-08/09、Metadata 增量与重扫已有源级/HTTP 证据；最终包/部署仍待验，不能只凭 feature 标记通过。
- [ ] **FIX-08 · P0：旧客户端过渡。** 已实现 s8/s9 双快照、精确协商和旧端 s9 写保护；
  派生 s8 只保留 WAV，保留已合入主表的厂商/Vorbis 搜索词；默认开启0.9过渡期兼容快照。
  SC源级与官方0.8.7 CLI配对通过（`d116dcd`）；关闭兼容配置拒绝同步并保留旧缓存。官方旧GUI的 `3562eea` 声明范围保留；fe13新GUI的迁移/离线和开发包另见1.4，不改写旧客户端配对身份。
- [ ] **FIX-09 · P1：按库提示与去重。** 已改用稳定库身份隔离升级通知，
  库身份/切换/去重已有源级回归；fe13实际GUI观察到同URL换UUID警告且旧缓存视图保留，未点击警告后的Sync，不宣称已替换到B。

验收说明：本轮已有 MG/快照结果证明当前迁移实现；客户端 mock 仅证明请求/回复包装，
把 WAV 行改成 `.flac/.ogg` 扩展名的夹具不能证明真实标签解析，benchmark 私有厂商表不能证明正式导入。
CORE-02/03 完成前不将“完整搜索范围扩充”或“完整 Metadata v2”标为已验收。

已有证据保留原源码/feature/环境身份，不改写成在 `02036a6` 上新执行：

- 服务端第五轮：Windows MSVC `1.97.1`；fmt、workspace/all-targets strict Clippy、
  test-hooks strict Clippy 通过；workspace 93 targets，1969 passed / 0 failed / 198 ignored。
  DB/server `migration9` 专项共 10/10；完整 MG 24/24，43.54 秒；MG14 再跑 3/3。
  私有任务证据 `schema9/logs/round5-source-identity.json`、`round5-workspace.result.json`、
  `round5-mg-results.json` 及各阶段日志保留源码 SHA256、命令和时间。较早失败仍保留。
- 客户端最终未提交输入：fmt、workspace/all-targets strict Clippy 通过；workspace
  1985 passed / 0 failed / 198 ignored，桌面测试 488 passed / 154 ignored；debug 0.8.7
  离线启动及两项原生关闭通过。私有 `client-ux2` 日志、变更清单及截图保留。
  未做 release 打包、portable 或固定安装，且当时未包含 #225 的完整服务端实现。
- 冷测交接曾记录 20/20、s9/s8 比值 0.807–1.079；它是原基准实现和 macOS 副本的历史证据，
  合并后搜索相关输入是否相同仍须核对。不能当成当前集成候选或百万记录绝对延迟的测量。

以下是上一轮 `bebf272` 候选的历史证据，不能替代当前 #204–206 新增行为的验收：

- fmt、受影响五包的 ASIO 严格 Clippy 通过。五包检查在 fixture 失败前有 51 个通过 targets、
  499 passed / 34 ignored；`bebf272` 修正后 DB 剩余 5 targets / 34 passed，
  desktop/remote/server 的 32 targets / 1147 passed / 159 ignored。按有效身份组合为
  88 targets、1680 passed / 193 ignored，覆盖选定范围，不是一次全仓全绿。
  首次 workspace 的 2008 passed / 2 failed / 198 ignored 原记录保留。
- 原生临时 HTTP 服务完成 s8 同步 → 显式迁移 → s9 完整同步、路径查询和 Metadata 增量 →
  离线缓存重开 → 回滚后重新同步 s8；`migration_snapshot` 1/1 通过，保留库身份、旧缓存字节和未知 Metadata。
- test-hooks 严格 Clippy 及迁移专项 2 targets / 11 passed 通过。MG 首轮 20 项通过，4 项 HTTP 受私有监督器日期类型
  错误影响；只修正 runner 后 4/4 通过，24 项均有通过证据。首轮失败与监督退出记录保留，Rust 源码未改。
- 官方 `v0.8.7` CLI（发布归档及二进制摘要已核对）对同一隔离库先完成 s8 同步，服务端迁移9后
  拒绝同步及写入；缓存 DB、`cache.json`、active 标记字节保持不变，临时服务正常退出。
  私有报告 `mg/runs/old-client-1cbab243aae949d4b8488d77cc3b01d8/report.json` 保留实际配对身份。

以上是上一轮 `bebf272` 及其输入的历史验收；日志、命令、源码/二进制摘要保留在私有 `audiolib-release-090`。
本轮 #204–206、当前缓存及兼容演练的证据在私有 `audiolib-204-206`，不沿用旧包作为新功能通过证明。
以下仍为 `bebf272` 的历史交付：Windows x64 ASIO构建、打包及portable7项通过；`validate-review-fixes.ps1 -CleanCheckout`
10 项、`validate-library-management.ps1` 10 项通过，均使用隔离合成数据，未打开真实工作库或音频设备。
固定安装核对 1252 个 package-owned 文件（更新13个）、CLI `AudioLibCli 0.8.7` 和隔离桌面启动通过；
portable 中文路径索引提示及安装后界面截图已检查。二进制源码身份仍为 `bebf272`，后续纯文档提交不改写该身份。

| 历史已验收并安装的 EXE（bebf272） | SHA256 |
| --- | --- |
| `AudioLibCli.exe` | `a468d19640b0b01fed103019d6aea3aa5a1641cd93d0e67507ef20cd60093b21` |
| `AudioLibExplorer.exe` | `e383a52466a0ac42a53ab4b9a0e98fc322112e8b94e7b22c85928e8051dda300` |

最初复审入口为 [Draft PR #226](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/226)，目标为
`feat/search-metadata-v2`，不执行 beta/main 提升。后台等待自动跟进已删除，原始 CLI 工作树保留。

### 1.3 同目录自动扫描的本轮验收

以下为Windows/MSVC1.97.1上的实际源级执行，合计99项通过；修正失败项后仅复验受影响范围，
各结果保留原来的源码差异和输入身份，不宣称一次全仓运行。

| 范围 | 实际结果 |
| --- | --- |
| 自动导入/扫描 | 自动来源单元7项、扫描控制单元2项、9个受影响DB集成目标64项通过；含真实CSV/TSV/BIFF8/OOXML、目录隔离、嵌入/表格去重、仅表格变更、新WAV、部分匹配保留、重排、暂停取消、锁、坏表和重提取保全 |
| 核心协议 | vendor相关10项通过，含新统计对象缺字段/未来字段兼容；原扫描计数不与表格计数混合 |
| 真实HTTP | 3个目标14项通过；新增s8及真实迁移后s9扫描→只改表格→revision/快照→RemoteProvider→停服离线重开；旧派生s8检索、新旧reader代次隔离和本地工作库/sidecars不变 |
| 实际厂商表格 | 用户提供的真实XLS只读副本识别109行、1个sheet；以3个表内文件名生成隔离真实RIFF/BWF音频，自动匹配3条、Description参与检索，第二次增量Metadata更新0。表格副本字节、生成WAV字节/mtime及用户标注均保全；不是原NAS音频全量扫描 |
| 桌面显示 | 扫描完成/运行时语言回归1项通过；fe13实际GUI的自动内容、重复扫描数字、P2与两个语言/主题观察另见1.4，不扩成所有GUI流程通过 |
| 静态检查 | 格式检查通过；Core/DB/Remote/Desktop/CLI实际ASIO配置严格all-target Clippy、DB独立no-default-features lib、server原生no-default-features all-target，以及修正后真实样本test target严格检查均通过 |

真实样本第一次验证在查询词采样阶段失败；测试现使用正式SearchRequest输入校验过滤未就绪词，
已选查询和Metadata字段、保全断言仍严格执行。复验通过并记录1个被拒绝的采样词，未更改真实表格内容。
上述单元/集成计数不等同最终payload、硬件或原生界面验收。
另有原生Mac `fe13b05` 的62项选定源检查通过：Rust/Cargo1.97.1、`aarch64-apple-darwin`、SDK27.0，干净隔离源码及Cargo.lock未变，保留原报告的shell nofile256记录，不倒改为后续子进程条件。
范围为DB自动来源8项、scanner2项、扫描集成9项、vendor32项、server真实HTTP8/9共2项、headless Vorbis9项；DB/server strict通过。
这是原生源检查，不是Mac客户端GUI、签名、实际包或部署。原音频filter零测试的记录保留，后续真正执行9项的结果单列，不冒充首轮覆盖。

后续独立Mac输入为fe13加六个Rust文件，patch `4d59457ae18fa80178d4938a31ef0ce9568565cf12813184b84aade01b126505`，含目录`d1c746fd`与Waveform`04c0892e`。Waveform28、目录整组20（含6个新增）及scope完整目标12，共60通过/0失败/0忽略；DB all-targets/test-hooks、DB独立NoDefault lib及Remote NoDefault all-targets三项strict通过。
六文件和依赖输入前后相同，旧62源码/锁文件/原收据不变，不合计成一次workspace运行。实测SSH/dev-env为256、pwsh原生子进程为10240，显式按后者验证，不声称256 FD预算；准备阶段的private caller失败原件保留，不扩称产品失败。
私有证据简名为 `mac-native-delta-b8d56fdac8e040c1a6e3b7092f9c8ba1/summary.json`，摘要 `6ea8fe189bedcef2646536e4999931a5c96658bac76e18bc44ac2d52901d0a72`。本轮没有MacGUI、包、生产部署、维护或性能测试；正式PG05五轮留release，最终payload仍待验。
Windows独立核心检查62项、一次fmt、DB/Remote默认特性all-targets strict及DB NoDefault lib strict均通过；输入为fe13加六Rust文件，delta `84455bfec37ddff4119295b79dde7bf5c619b5810810d026038fd697ac1b1540`。私有证据 `windows-core-functional-94f749889e6b4b4f87160b4d17baa0d7/result.json`，摘要 `511b05fc5643b2323f30f6613e195c7e48a11105f41788b713aa25929e46c29f`。同机另有外部任务，本轮仅证明功能，不给性能结论；不与Mac60项累加。

实际NAS额外验收使用用户指定目录的109个真实WAV及1个XLS，在新隔离库中完成109行全部匹配、0歧义/未匹配/错误；
首次Metadata更新6条、103条与嵌入内容去重，增量109个WAV未变、Metadata更新0，109个公开AudioFile DTO一致，3个Metadata查询命中。
13条CLI命令均exit0；109个源文件名/大小/mtime、XLS完整SHA及3个WAV抽样SHA前后相同，抽样共11,122,658字节，没有全量散列600MB音频或打开工作库。
这与前述真实XLS副本/3个生成WAV测试是两次不同的验收；真实路径、表格内容和日志只保留在私有证据。

### 1.4 当前Windows开发包、固定安装与选定GUI

该次已交付客户端源码身份为 `f70156e0a54dddb812584d24564f2601fe6e4ebe`，运行版本为0.8.7；build/package/portable-accept/install四阶段通过，不回标为当前0.9.0冻结候选。
双二进制服务端包的既有验收身份仍为 `fe13b05f43cc75ae3de6373aedc390d4afdf2858`：显式release benchmark helper、production server+migrate build/package/native-accept四阶段通过，包含无test-hooks保护和owned8→9→8迁移，不回标为f701服务端包。

| 开发包EXE（源码身份） | SHA256 |
| --- | --- |
| `AudioLibExplorer.exe`（f701） | `1193cb6ca13a1ff3594b449a8c66f122ab518b7e03bd3160d346247e8cd2e962` |
| `AudioLibCli.exe`（f701） | `41e28e816b4ab6c5d39baf55bbeeb8b45d4aa2a562a3824f8b7352728028dbd0` |
| `audiolib-server.exe`（fe13） | `e050021f3f6d33224cc548b39bfc3d367969ecf1439002d752f6f7b2324dbf2b` |
| `audiolib-migrate.exe`（fe13） | `22c6b07e6347485804c8bf64821f3acf0f729506eda6c73a4373822fec4f319c` |

主工作区唯一固定 `local_release/` 的907个package-owned文件与f701已验收包一致，更新9个文件；CLI0.8.7及隔离桌面启动通过。交付证据为 `delivery/runs/f70156e0a54dddb812584d24564f2601fe6e4ebe/state.json`，摘要 `2fbe4267d1afd519ed39c41d7b08980421b2fc2d1c2988d895fd8fa48297db8b`；固定安装记录 `local-install/f2f2e31191d7475eab7661effc10c03d/report.json`。
手动验收经该目录 `Start-AudioLibExplorer.cmd` 启动并隔离userdata/IPC；本轮f701仅实际观察到本地 `Path index built`。原生文件选择对话框工具阻塞，手动BIFF、重复同步及Waveform GUI补验未完成，保留release门禁；以下选定观察仅属于此前fe13身份，未记为本轮最终GUI通过。
安装未打开或复制日常工作库、偏好和缓存；这次交付是开发候选，不覆盖已发布v0.8.7资产，也不是最终0.9.0/main/tag产物。

| 选定原生GUI范围 | 实际观察 |
| --- | --- |
| 新格式播放 | 连续FLAC→OGA→OGG复用输出，Waveform可见、Ended0.080，无原加载错误；静音观察，不声明硬件出声或延迟 |
| 同目录自动表格 | 新2个WAV由GUI首次扫描，库74→76；保存独立用户标签后加入两行CSV。嵌入等值描述只显示一次，新增描述Main命中1条，标签保留 |
| 重复扫描数字 | 实际显示0新增/0更新/0缺失/2未变/0问题、1表/2匹配行/0Metadata更新/2未变/0未匹配/0歧义；总数76。第二次扫描的updated1未直接留存，不能从后端结果补成GUI观察 |
| P2重预览/模板 | BIFF English页表头2→1→2后仍选English；恢复3个匹配及Filename/Description/Keywords。实际保存模板使用English映射；当前模板契约不持久化sheet/header_row，二者的GUI稳定性与映射落盘分别记录 |
| 主题与关闭 | Defaultv2英文/中文内容和布局可见；正常X关闭、短暂“正在保存更改”后窗口及目标进程消失。未捕获GUI exitcode，不把进程消失写成exit0 |
| 远端迁移/离线 | A初始schema8/74条，经GUI迁移74/74完成，live GET确认schema9；Path查询2条、自动扫描后76条和新词Main命中1条。正常关闭及停服后固定安装冷重开，离线缓存仍76条/新词1条 |
| 同URL换库 | B的新UUID被识别，Retry后显示更换身份警告，旧A缓存76条及新词1条保留；auto_sync=false，未点击警告后的Sync，不宣称已打开B快照 |
| A/B生命周期 | 两个owned production server会话均经专用console Ctrl+C正常排空，exit0及normal_stop_verified=true；A保存的startup schema8不是迁移后的实时schema9 |

GUI回滚、management-off/reader权限界面及B警告后实际替换未执行；正式HTTP与production服务包验收是独立证据，不能把这些GUI项记为通过。
五条Waveform替换警告已定位到同文件多个SQLite连接的生命周期；首版`4ec`输入保留原Windows os5失败及27项定向通过，后续`04c`尾部修正28项已在Mac和Windows通过。
这些修正及服务端目标常量已进入f701客户端开发包并安装；两平台核心检查及包验收不代替本轮GUI警告复核，不将此前fe13观察改称当前GUI通过。

此前fe13的最小私有证据身份：Windows `accepted-payloads.json` 摘要 `b179acb8adacb02be28cb5748defb05a4fb1b7e5690f4b69a0932ce93f7803eb`；
Mac `mac-native-fe13-fd8728591e9b4e6bb087cd454f7a8212`；实际NAS `nas-sidecar-b50ef7c7435b4adeab9e98b92858f8e9`；
GUI `native-gui-selected-final-b5ce5a1e4bf0486495aa23b3f5a66610.json` 摘要 `d827c04d6d51269bbdc898f980fdbbb51c45222da7565e34eb108734a0d83bd6`。

### 1.5 核心范围代码审查

2026-11-05 对 `cbb2f884c989a3ef89dafd653207ec3554f075b9` 相对 feat 的候选改动做定向 CR，
覆盖增量扫描与自动表格来源保全、schema8/9 准入及缓存发布、Metadata 提交与回滚，
以及最近 All/Path 续页计数和共享 Waveform 连接生命周期；未发现可确认的新增 P0/P1/P2 代码问题。
这次仅审阅源码并复用已有验证，没有新增 Cargo、GUI、性能或全局回归结果，PG05 和未完成 GUI 门禁保持原状态。
测试程序在后续复查时已无目标窗口、GUI 或启动器进程；未观测其退出码或方式，未发送强制终止。

导入入口按实际能力描述：桌面原生文件选择器当前列出 CSV/XLS/XLSX，文本解析支持 tab 分隔；
后端手动解析和服务端目录预览另支持 TSV/TXT，不将其等同于原生选择器可列出的格式。
普通 WAV 扫描自动发现同目录 CSV/TSV/XLS/XLSX。该说明修正不改变应用二进制或已验收包身份。

## 2. 存储目标与当前实现边界

程序版本、API、常规本地写目标和实际服务端库格式分别记录，不用一个 schema 常量替代所有含义：

| 对象 | 当前源码现状 | 剩余边界 |
| --- | --- | --- |
| 程序 / HTTP | Cargo `0.9.0`；API `/api/v1/` | Cargo/锁文件/当前说明已统一冻结；最终包与main/tag身份待验，本阶段不创建标签 |
| 普通本地可写库 | `SCHEMA_VERSION=8`、`TARGET_SCHEMA=8`；本地路径索引构建与重建保持真实 s8 格式，重开保护已覆盖 | 普通本地写入不支持 s9；不能将路径索引准备称为主库升级9 |
| 显式服务端迁移 | `audiolib-migrate` 或管理 API 将完整 s8 发布为 s9；专用 `open_server_existing` 可重新打开 s9；Metadata 和根 relink 已有回归 | restore/maintenance 保持 schema8 保护；实际生产部署及平台验收另行执行 |
| s9 完整格式 | `ale-s9-cld1-t3-m2-p1-a1-sd1`；主 FTS5 tokenizer 为 t3，目录修订为 sd1 | 检查真实 schema/features/结构/身份；`source_search_rev=3` 与三项 feature 原子发布；保持原主 FTS5 不变 |
| 客户端只读快照 | 支持完整格式校验的 schema3..9；DB的`MAX_SNAPSHOT_SCHEMA`直接引用Core的`SNAPSHOT_SCHEMA_MAX`，避免读上限漂移；s9 原样只读打开，owned staging 检查目录 FTS5、成员、runs 覆盖及无脏队列；真实 HTTP 同步和 Metadata 增量已通过 | 不把 s9 交给 schema8 修复器；未来/不完整格式拒绝并保留旧缓存 |
| `/capabilities` | catalog2、API v1，按实际源库和角色报告读写格式、迁移、四种 scope 和厂商导入限额；普通客户端本地写格式仍为8 | 真实服务角色/格式及旧 JSON 形状解析回归已通过；正式版本及原生部署另验 |
| s8 派生快照 | 已实现 s9/8 双缓存与精确协商；8派生只含 WAV、移除目录与厂商自有表，沿用已有主 FTS5 的 rev3 词和库身份；新端优先9 | router 配对、修订变化及关闭兼容选项回归通过；官方0.8.7 CLI 与 `d116dcd` 的真实兼容配对通过；官方旧GUI在3562eea的声明范围完成；fe13包验收见1.4，最终发行payload的旧端配对待验 |
| 厂商导入 / Vorbis | 真实 CSV/XLS/XLSX、本地与服务端完整导入/回滚和 FLAC/Ogg/OGA 提取、重扫、搜索已接通；不支持的服务器隐藏入口 | 较早29项与后续受影响32项目标分别通过，真实 HTTP/离线链路和解析资源演练通过；PG05、未执行的原生交互及最终发行payload待验；fe13选定GUI和开发包验收见1.4。Opus 仅标签，无原生解码 |
| AudioFile COW | `AudioFileData`保留原字段、类型和serde属性；`AudioFile`以私有Arc持有整条记录，Clone共享、修改时分离，手工serde仍输出原JSON | **公开Rust源码接口变化**：构造使用`AudioFileData`后转`AudioFile`，移出字段先`into_data()`；JSON/API格式、数据库schema、`AudioMetadata`及`attributes: Value`不变。直接消费者源级检查已通过，实际性能及最终包仍按新身份验收 |
| 服务端交付布局 | 新包包含 server 与不带 test-hooks 的离线 `audiolib-migrate`；仍为七归档十四资产 | layout_revision=2、automatic_upgrade_compatible=false、manual_bootstrap_required=true；minimum_updater_version=0.9.0。冻结0.7.8旧政策真实拒绝，新政策合成验证通过；首次手动引导，当前0.9.0冻结候选尚未发布 |

目录索引自有 SQL 触发器捕获旧 schema8 写入产生的目录实体/层次及音频成员变化，不依赖新增
scalar 函数。脏队列使 readiness 失效，已有内存缓存也不能绕过；新写入者按目录增量消费，
索引、清队列和 generation 同事务提交。notes/tags 不标脏，统计变化只使 generation 更新。
NULL 归属行分开 runs，避免被路径范围误命中。缺捕获机制的旧 s8 索引须重新准备；完整的
历史 s9 无捕获对象来源仍可只读校验。对象安装/删除核对定义与归属，未知扩展保留。

`-z1` 表示 Metadata 列存储格式，不是快照外层 zstd 传输。
不能因为发送 zstd，就宣称实现了 s9 `-z1` 格式。VN-08/09 的现状和目标区分见
[测试计划第 7 节](TEST-SEARCH-METADATA-V2.md#7-版本协商与提示)。

## 3. 两端兼容与迁移状态矩阵

下表的源码与隔离 HTTP/官方旧 CLI 配对已有相应证据；正式 payload、实际 GUI、真实平台和部署仍分别验收。
旧端取得的是在自有副本中验证过的真实 s8 格式，按接受格式发放；源 s9 不改写。

| 客户端 / 服务状态 | 读取、搜索与缓存 | 写入与提示 |
| --- | --- | --- |
| 旧 0.8.7 → 原 s8 服务 | 沿用已有路径，保留旧快照 | 沿用已支持的 s8 写契约 |
| 新客户端 → 原 s8 服务 | 基础同步继续，主字段与仅文件名可用；缺服务能力时隐藏迁移/厂商入口，路径按实际索引 readiness 开放 | s8 写入仍须通过现有身份与权限检查 |
| 旧/新客户端 → 新二进制、库仍 s8 | 基础读取和旧快照保持可用；新功能只按实际能力开放 | 不因换二进制自动视作库已升级 |
| 回填中 → 客户端快照查询 | 主字段/文件名继续可查；无就绪目录索引时All回退主字段、Path显示待准备。RemoteProvider日常查询在已校验的本地只读快照执行 | 普通支持写入可继续；管理迁移状态可读，客户端显示阶段及已提交计数 |
| 服务端在线查询 | 已有GET/POST `/api/v1/search`，经迁移准入和DataProvider搜索；与客户端快照查询是两个执行入口 | 普通路由30秒超时返回408；迁移尚未就绪时Path返回503/Retry-After 30，All可回退Main并带不可用标记，不称“预留路由” |
| switching | 读取继续；保留当前缓存，拒绝混合代际状态 | 写入 503 `migration_switching`、Retry-After 30；按原请求语义重试，避免重复提交 |
| 新客户端 → 完成 s9 | 精确接受9时总是取得9；完整校验后只读打开，真实全量同步、四种范围、标签/导入搜索、离线重开及回滚重同步有源测试证据 | 客户端快照仍只读；写回经服务端身份/角色/库格式准入；Metadata 与 owned cache 增量已测，导入/回滚后提示刷新 |
| 旧 0.8.7 → 完成 s9，兼容开启 | 只接受8时发放 WAV-only s8，保留已合入主表的厂商/Vorbis词；默认在0.9过渡期保留 | 旧8写契约不能因拿到派生8就写源9，服务端拒绝不匹配写入（409）；给出升级头与能力字段，不承诺旧 UI 呈现新增提示 |
| 旧 0.8.7 → 完成 s9，兼容关闭 | 明确拒绝同步，保留最后已验证 DB、cache 状态及 active 标记 | 旧端写入仍拒绝；关闭配置须由管理员在过渡结束后显式决定 |
| 回滚后 → s8 | 重新确认格式、revision 和库身份，失效旧请求，重新同步；这不是恢复旧数据备份 | 不复用旧 s9 写准入；新写入仍走实际8格式/权限保护，不覆盖工作区、标注或偏好 |

离线、取消、校验失败及不支持的未来格式均须保留最后一份已验证缓存。
新快照只有完成身份、结构与完整性校验后才能发布给 UI；服务端模式不能迁移客户端本地工作库。

## 4. 分阶段执行方案

以下为部署前可审阅方案，当前不执行生产步骤；地址、token、根目录及配置只由私有环境提供。

1. **冻结交付范围。** 完成 CORE-01..03、本轮修正及其验收，记录最终代码、配置、工具链和候选身份。
   按 catalog2、客户端读3–9/普通本地写8、显式服务端写9的边界准备候选，保留主题、工作区和数据保护。
   最终版本统一前不标正式0.9.0。双快照默认开启，不将旧端过渡能力静默删除。
2. **在拥有的副本演练。** 从只读备份输入生成隔离库，登记 s8 格式、library_id、业务记录/标注、
   扫描历史和主 FTS5 基线；验证额外可用空间至少为库大小的 1.3 倍。
   使用隔离随机端口及合成/副本配置，不连接生产或现有 beta 服务。
   完成迁移、取消/恢复、进程退出恢复、重开、完整快照同步及回滚演练。
3. **先准备客户端。** 先交付能连接原 s8 且实测能读 s9 的客户端；保留服务配置、缓存、
   本地库路径和个人工作区。本地路径索引提示可以“稍后”，不能据此改写服务端库或宣称本地 schema 已升 9。
4. **手动引导安装新服务，暂不迁移。** 旧更新器不能接受新增迁移工具的包布局，首次0.9必须手动安装；
   不用放宽冻结旧白名单绕过此限制。核对新布局声明、两个二进制 hash、launcher、配置、数据目录和提交，
   保留旧二进制与备份；此步仍须部署授权。当前 `AppState` 启动调用 `open_server_existing`，不会主动执行 s8→9；
   schema 8 仍走普通已支持的打开路径，较旧库原有迁移另有行为，不能据此保证所有启动均只读。
   `AUDIOLIB_DEFER_SCHEMA9=1` 仅被迁移器 `startup` 动作消费，不能代替真实启动和库版本检查。
   当前 HTTP 管理动作不接受 `startup`。未显式迁移前核对仍为8、健康检查及旧客户端可用。
   仍需旧端使用的窗口中，先不启动非 WAV 补扫；完成9及双快照验收后再启用新增来源。
   布局2把 server 和 migrator 作为同一候选，两个文件都须通过摘要核对后才启动；自动更新不得接受缺少迁移器的当前布局。
   beta 刷新和通用部署脚本的成对安装/失败恢复已通过66场景合成验证，Mac 原生部署尚未验收。
   `--with-db` 还须先证明旧 beta 服务已正常排空并保留其一致备份，才能替换 beta 库及旧 sidecar；
   库格式已变化或无法确认时，不执行仅二进制降级。launcher、配置和文件访问授权须保留。
5. **显式触发并监视。** 授权后，通过支持的管理接口或 `audiolib-migrate start --db <owned-copy>`
   开始；实际工作库路径只在批准的私有运行配置中提供。先确认唯一 owner、权限与库身份。
   监视 backup / path_build / optimize / switch，保持回滚备份；状态 paused/failed 时停止后续推广。
   不删除锁侧文件，不强制终止占用者，不在迁移中 VACUUM。
6. **完成后验收并同步。** 核对实际 `user_version=9`、sd1 格式、三项 feature 和修订，
   确认目录/runs 覆盖、主 FTS5、业务投影及未知 Metadata 保留。
   由新客户端完成真实快照下载、重开、路径查询、播放与受支持写入；核对记录数及库身份，
   保留配置根外不可播放的既有边界。用官方旧客户端另验 WAV-only兼容同步、旧写拒绝、修订后刷新；
   再演练关闭兼容选项的拒绝/保缓存，不把新客户端模拟旧头当作真实旧二进制验收。
7. **推广与交付。** beta/main 提升、标签、正式构建和发布须另获授权；各最终 payload
   从最终身份构建并验收，固定安装同步完成后才报告交付完成。

### 回退与恢复

- 切换前：pause/cancel 在持久批次边界生效；确认终态及 s8 可用，保留 checkpoint/备份供诊断或恢复。
- 切换后：当前目录迁移器支持验证备份后、持独占管理与扫描 lease 的逆向事务回滚，
  保留备份后的业务内容及普通注释，删除目录派生机制和9标记；当前实现不会把厂商正式数据或新增格式音频变成旧写者完全理解的数据。
  必须按最终 recipe、实际新增功能及原生包再验证，不能把 `user_version=8` 等同旧二进制写入保真。
  不将历史设计中的“直接恢复整个备份”作为默认操作，否则可能覆盖新写入。
- 若新增 Vorbis、厂商字段或其他 s9 业务数据后不能无损退回 s8，停止自动回退，先评估和保全数据。
  执行前须由管理员通过受支持预检/数据审查确认；当前迁移器不自动证明旧二进制可安全写入。仅降schema或保留注释不代表旧写入者理解新增业务。
  不向旧二进制交付不支持的库，也不靠删除新增业务数据获得兼容；完整备份恢复另需授权及写入排空，并明确备份之后数据的处置。
- 所有回退保留源 WAV、原备份、Metadata、标注及未知扩展；不以重扫代替数据保全。

## 5. 开发定向验证与正式发布门禁

集成候选后续修改由实际风险选择检查；已有全仓结果不能覆盖新合并行为。
开发阶段仅补#204/#205/#206与Waveform受影响的定向检查、直接消费者和必要的数据保全；保留迁移、取消/恢复、写入排空和源文件不变断言。下表的百万性能、全平台及全局回归属于正式发布阶段，失败或未验不得记为通过，也不阻止核心功能完成定向验收后先交付开发版。

| 阶段 | 所需证据 | 当前 |
| --- | --- | --- |
| 契约与文档 | schema/features/能力/Retry-After及版本规则一致 | 本地写8/专用服务端写9/快照3..9分开记录，catalog2及switching30统一；发布checker AST与12个合成Git门槛回归保留原通过证据。当前Cargo/锁文件/说明已冻结0.9.0。本次版本预检失败：checker不识别v0.8.7的`SNAPSHOT_SCHEMA_MAX = TARGET_SCHEMA`声明；须修复并重验，实际包及其余发行门槛仍待完成 |
| 受影响源码 | fmt、strict Clippy、相关测试及直接消费者 | COW五包ASIO strict通过；Core187及DB lib326通过/5忽略。原Vorbis失败保留且修正后完整通过；后续DB strict、server NoDefault strict通过，44条目标调用1225通过/0失败/165忽略，复用此前未受影响结果，不累加为唯一个案总数 |
| 缓存读取 / 成对更新修正 | 取消/提交交错/连接复用、包缺件和升级恢复 | 隐式读取11条目标调用96通过/0失败，含server NoDefault updates3项；fullfmt与DB test-hooks strict通过。updater all-target strict及33通过/2忽略，覆盖成对安装/缺件拒绝/第二文件失败恢复。Unix执行位仍须匹配平台验收，shell成对部署另验 |
| 成对部署 / beta库刷新 | 匹配双bin和验收收据、停止排空、一致备份、失败恢复 | Git Bash独立语法检查及66场景全部通过；覆盖缺件/错hash/错receipt、第二文件失败、旧迁移器不存在、标记恢复、持续迁移保留整对、停服失败不启动第二实例、排空后schema变化拒换、真实owned SQLite/WAL逻辑备份与失败保护。只使用fake服务和隔离SQLite，不代替Mac launchd/TCC/真实根目录及平台验收 |
| 迁移安全 | MG24、跨进程控制、备份、取消/恢复、切换拒写 | 完整MG保留 `3562eea` 的24/24、41.55秒身份；fe13 Windows production包另有owned8→9→8通过。均不等于实际VM断电、全部文件系统或生产规模owned副本迁移门槛 |
| 两端真实 HTTP / 旧客户端 | 全量/增量、离线、回滚、实际官方旧二进制 | `2db7efd` 的5个同步目标16项、rooted7项通过；官方0.8.7 CLI与d116dcd配对通过。官方0.8.7原生GUI对3562eea完成O1–O12声明范围；独立旧写HTTP409、拒同步保缓存和冷离线重开有证据，均不是新候选正式包验收 |
| 搜索正确性 / 解析资源 | 完整oracle/分页/深目录/NULL、表格RSS/边界、恢复 | fe13 run81110616完整八范围oracle及所有续页通过，203.778秒；18项边界、10k/200k CSV/XLSX、近50MiB有效BIFF及真实退出恢复保留原身份证据，不与新运行重复累计 |
| 百万记录性能 | 绝对P95<500ms；PG05五轮中位数倍率≤1.1 | **BLOCKED**。最新fe13 run81110616正式五轮全部充分重叠，五组倍率1.19118/1.20732/1.19277/1.20290/1.21311，均失败。warm100+facets P95=209.4968ms通过；首次search/facets各自通过，但case1/6合计575.4356/547.2962ms失败。保留旧53276831、0c95367b及诊断，不用绝对延迟代替倍率 |
| 桌面交互 | 范围、导入、迁移、关闭与主题 | fe13固定安装选定GUI实际通过范围见1.4；回滚/off-role界面和B警告后Sync未执行。旧GUI、源级异步回归与新GUI分开，不宣称全界面清单通过 |
| 原生文件系统 | 根句柄、复制/替换、权限与限额 | Windows本地NTFS7项、自有SMB/UNC6项通过，另1项创建链接返回OS1未验；旧Mac62项含Unix根/父目录/源文件/preview链接替换防护及原shell256记录；后续六文件delta60项的原生子进程实际10240，均不等于MacGUI/TCC、Linux或全部文件系统 |
| Windows实际开发包 | 客户端ASIO；server+migrate；许可/隐私/启动/数据安全 | f701客户端build/package/portable-accept/install通过；fe13 production服务端四阶段保留原身份，见1.4。client479/server260许可闭包及53项Docker合成检查不等于实际Docker运行，最终发行payload待验 |
| 固定安装 / 最终身份 | 所有package-owned文件SHA、CLI版本、隔离启动 | f701主工作区907文件/9更新、CLI0.8.7及隔离桌面启动通过；核心GUI补验因工具阻塞未完成，保留release门禁。最终0.9/main/tag及发布后摘要同步仍待完成 |

最新正式性能身份为 `fe13b05f43cc75ae3de6373aedc390d4afdf2858`，run `81110616bd274a5fa9a07e58483887da`，
以私有 `compact-summary.json` 定位；1.198GB原始样本保留，不复制到仓库或覆盖旧报告。
八范围完整集合和续页oracle通过（203.778秒）；8个fresh-first请求的search/facets独立通过，
但case1与case6两阶段顺序合计分别575.4356ms和547.2962ms，未达到合计500ms门槛。
warm100含facets的P95为209.4968ms、结果页P95为0.0365ms。OS cache未purge，不能称真实冷缓存。

| PG05正式五轮 | 空闲p95中位数（µs） | 并发写入p95中位数（µs） | 倍率（上限1.1） |
| --- | --- | --- | --- |
| Main / relevance | 6.8 | 8.1 | **1.19118，失败** |
| All / relevance | 8.2 | 9.9 | **1.20732，失败** |
| Path / 过滤与Filename排序 | 8.3 | 9.9 | **1.19277，失败** |
| strict Filename / Filename排序 | 6.9 | 8.3 | **1.20290，失败** |
| 空中文 strict Filename / relevance | 6.1 | 7.4 | **1.21311，失败** |

五轮均 `sufficient=true`，仍执行真实导入与回滚、原有预热/空闲及真实写入重叠要求。
所有倍率失败，不能因缓存命中、微秒级绝对延迟或后续phase原型宣称正式PG05通过。fe13后新源码另有身份，不回标此轮结果。
后续私有phase `67a`自然exit0、真实三批导入后回滚，但计时受新后台CLI并行编译污染，不计作PG05或新性能通过，也未证明剩余根因；当时`guard-v2`仅准备、未执行。

较早的主性能诊断 run `b3df63be340b4718af8c0d66ef85733c`，源码 `3562eea`，
`diagnostic_only=true`。warm100+facets P95=213.9338ms、结果页P95=0.1073ms；严格PG05仍阻塞。
它使用原 `d116dcd` 五轮导入/回滚后的百万记录语料，经只读SQLite online backup复制到新owned目录；
原DB/WAL/报告保持不变。OS cache未purge，fresh process/provider不称真实冷缓存。

| PG05请求 | 空闲p95（ms） | 写入p95（ms） | 单轮倍率（上限1.1） |
| --- | --- | --- | --- |
| Main / relevance | 0.0407 | 0.0410 | 1.00737，在限内 |
| All / relevance | 0.0563 | 0.0498 | 0.88455，在限内 |
| Path / 收藏+48kHz / Filename排序 | 0.0539 | 0.0401 | 0.74397，在限内 |
| strict Filename / Filename排序 | 0.0318 | 0.0530 | **1.66667，未过** |
| 空中文 strict Filename / relevance | 0.0124 | 0.0109 | 0.87903，在限内 |

只达到4/5个单轮诊断边界，不能宣布正式PG05通过。phase `441df` 的分段诊断发现整条DTO深克隆开销：
Main的JSON约400字节且未变化，Clone仍由约20.6增至36.5微秒；Filename JSON约400增至3806字节、
包含约51行vendor数据时，Clone约20.7增至46.7微秒。这支持随后实现的**整个AudioFile COW**，
不把attributes-only原型视为已经解决瓶颈。临时phase计时模块已按原字节精确移除；
最新完成的 run `64b81806b51d4f3e9cd8de46fa1b3c3b` 使用已提交COW源码 `20c4024`：
warm P95=216.5822ms、结果页P95=0.043ms；五类写入P95依次0.0101、0.0121、0.0122、0.0102、0.0093ms，
倍率1.14773、1.17476、1.14019、1.15909、1.14815，严格门槛仍未通过。
其后2KiB内联查找键在 `0628cc4` 通过61项定向回归；run0c95367b完成八组完整集合/续页及warm100，
run53276831完成五轮PG05但五组比值仍未过1.1。每轮真实导入均取消后排空并完成回滚，原始样本和失败报告保留。
隐式读取修正的后续单轮run `68174015fe334b9db2f52388752c27ee`，helper在0628cc4加已冻结DB输入上编译，
该输入随后提交为9102197；保留129项编译输入及摘要，不把提交动作说成新编译。
warm100+facets P95=257.419ms；Main/All/Path/Filename/空中文写入P95分别0.0092/0.0111/0.0111/0.0093/0.0084ms，
比值1.15/1.13265/1.11/1.14815/1.16667。它是完成的单轮诊断，正式五轮尚待新身份执行；原DB/WAL未变。

原有失败和计时身份继续保留：`0b3f2f7` 的a58单轮warm227.8722ms/结果页0.1408ms，
五类倍率依次1.35794、1.77593、1.41844、1.49294、1.32733；`2c45697` 的f2eaba诊断亦未达1.1。
较快查询曾触达50000条采样保护上限，`41401b`中止夹具不复用、不算通过。
私有helper现两侧均在public search返回即停表，校验/记录在计时外；较快查询再次触达500000保护上限后，
仅将有限样本保护上限提高至2000000，保留该纯runner修改的前后源码与收据；
180秒截止、完整oracle、实际导入、取消/join排空、至少3批和每类30条真实重叠样本、正式五轮与1.1门槛均保留。
批次work interval包括取数、投影/预维护、提交和状态，不是精确SQL持锁时间；未对旧结果追溯重算。

较早结果继续保留原条件：`d116dcd` 百万记录100查询P95=112.537ms，扩展 warm P95=443.843ms/结果页329.371ms；
原五轮 PG05 失败。`fd2e04e` 的 run `07c5d07da8724b568898f13cb61fcc20` 完整 oracle通过，
warm729.5957ms/结果页521.2298ms及五轮 PG05均失败；`ffcfbce` 的 scoped SQL去重随后改善结果页。
`0b3f2f7` 之后曾因私有 helper 将 All 首屏下界201误当精确总数，遇到合法精确50000而断言退出；
原失败夹具/报告保留，改用可信完整 oracle并保证校验错误先取消/join后，才有上述 a58完成诊断。
这些运行的源码、物理FTS/历史状态及计时版本不同，不用前后耗时单独推断某缓存导致退化，也不相加为一次通过。

本轮涉及扫描协调、Metadata保全、导入/relink、关闭及格式播放。fresh accepted portable就绪后，
保留五个现有数据安全/真实二进制回归：`validate-review-fixes.ps1 -CleanCheckout`、
`validate-library-management.ps1`、`validate-access-denial.ps1 -CleanCheckout`、
`validate-desktop-close.ps1 -Cycles 3`、`validate-playback-ipc.ps1 -CleanCheckout`，均显式指向该 portable。
实际 GUI 单独协调窗口，避免与性能/服务器竞争；脚本的合成数据、NTFS ACL恢复、正常退出/排空边界保留。
真实 legacy fixture、新增格式播放和平台/硬件覆盖分别报告，不用旧 WAV-only 或模拟结果替代。
ASIO 使用已验证 SDK 和实际发布配置，不再追加无关的非 ASIO 全矩阵。

交叉复核另修正厂商任务状态：无完成回执且已开始回滚的任务不能再续提旧commit断点，
准入拒绝保留原rollback诊断、进度及来源；已有完成回执仍按原键返回不可变历史结果。
新增真实部分回滚失败和首批回滚失败两项回归，重复拒绝后仍须能完成回滚恢复。

并发修正的数据库包曾完整运行58目标、581通过/0失败/34忽略，保留实际测试EXE身份。
之后的扫描入口与Windows报告锁分别按受影响目标复验：真实FLAC/Ogg/OGA热查询重提取2项、
扫描边界/控制各4项、厂商32项，以及上述迁移25项均通过；不是一次最终源码的全仓全绿。
远程包级297项通过，另有1项更早定向观察，不能重复累加为298个唯一用例；服务端早前目标、
修复后原失败5目标25项及余下7目标42项分别保留身份。
`ffcfbce` 的57个DB顶层目标587通过/34忽略，server2目标7通过、fmt/DB strict通过。
`0b3f2f7` 对应源检查的DB lib曾309通过、1个取消钩子用例失败，修复后定向8个取消及5个controller通过；
其余56个DB目标294通过/29忽略，server2目标7通过，fmt/DB strict通过。
较早pool/快路径/排名去重检查的35项单元、123项DB集成及7项HTTP，合计165通过/0失败，
对应后来的 `2c45697` 实现；这些历史执行有重叠，不继续累计成workspace总数。
当前31文件COW输入在3562eea基线上：选定core/db/remote/desktop/cli五包ASIO strict已通过；
Core187通过（含6个新COW行为测试），DB lib326通过/5忽略（含真实provider/cache COW回归）。
选定测试已完成59个目标823通过；第60个vorbis_scan目标为1通过/1失败，累计观察824通过、1失败、34忽略后中止。
失败位于 `vorbis_scan.rs:250` 的阳性cache_hit断言；它尚不证明结果或Metadata陈旧，也不能改写成通过。
随后修正扫描完成事务：IMMEDIATE准入后，在同一实际最终图像中准备canonical页面，成功提交后经原有Dw/revision证明发布；
不重标旧DTO、不放宽原断言。`owned-file-validation-finalize-bf7bb9968ca04a65b54960d0816f1393` 完成复验：
取消/缓存/扫描/Vorbis/Waveform相关47通过；Desktop/remote实际ASIO的8条目标调用809通过/162忽略；
server NoDefault的25条目标调用369通过/3忽略；DB及server strict通过，源码前后身份一致。
合计44条目标调用1225通过/0失败/165忽略；过滤DB-lib重复使用同一binary，部分扫描目标与先前结果重叠，不能直接与823相加。
此前Core、CLI和未受扫描收尾影响的DB结果保留原始输入/特性/二进制身份，形成组合证据，非一次完整workspace重跑。
以上为较早COW源码复验历史；其后fe13开发包、固定安装及选定GUI的实际交付见1.4，不倒改这些检查的输入身份。

最小证据定位：MG `final-3562eea-full24-42dd609abce047f8a2e5b4644cfe76d7/result.json`；
官方旧GUI `old-gui-e9a8c716ab574d98b2fee7a2d9b82bcd/native-gui-final-receipt.json`，绑定源码3562eea、
官方0.8.7 GUI SHA256=`859541a34d4eb05a04ddcda4f92d88596253235bc8ad1ed66a723999489faf81`。
旧GUI在隔离服务中s8初次449条→迁移后WAV-only派生8的409条，vendor搜索通过；
GUI禁止编辑/保存，不声称它发出禁止的PATCH；独立同旧契约HTTP写入409且服务器/缓存未改。
兼容关闭后同步明确失败且DB/cache字节保留；服务端正常停止后，官方旧GUI冷重开离线vendor搜索通过，全部正常退出0。
它完成O1–O12声明范围，不覆盖新候选GUI、正式server包或其他平台。
官方旧CLI证据仍为 `old-client-ed106a3ddcf6429691f204efbea81a5f/report.json`（candidate=d116dcd，
旧归档SHA256=`2ee3f3984e570ffadfe62ec052be2d0707e2845e0aa003434b2f337267fa6ba7`，
旧CLI SHA256=`f6fcd7d803311712d96ca704535dcafc5428d538fd770af1374749991fc7fb6f`）。
性能为上述b3df的 `search/pg05-probe-result.json` 与独立warm报告；phase441df仅用于归因。
COW检查保留 `owned-file-validation-*` 的实际输入/特性/EXE及停止位置，修正后重验使用新身份；
既有报告及计时修正收据不覆盖，不将变更的helper重绑定为旧编译。

性能目标仍为 **P95 latency <500ms for 1M+ records**；s9/s8 相对闸门为原约定的
每查询五轮、每轮 p95 的中位数不超过 s8 ×1.2；PG05另要求并发/空闲五轮p95中位数≤1.1。
绝对延迟、s9/s8及并发倍率分别报告，缺少真实冷缓存/平台条件时明确未验。

剩余发行条件：严格PG05正式五轮及首次组合延迟、后台CLI第二轮稳定输入复核、f701核心GUI警告复核及最终发行payload、最终main/tag身份冻结、
Mac客户端原生GUI/签名/实际包及匹配Mac/Linux/Docker平台交付、生产规模owned副本迁移/恢复与配对门槛。
已完成的f701 Windows客户端包/固定安装和此前fe13选定GUI不再列为全盘未验；本轮工具阻塞未完成的GUI补验及其他未执行交互与后端正式证据分别记录。
方案仍为客户端先就绪 → 手动引导新服务、库保持8 → 管理员显式迁移；双快照窗口结束及生产执行另行决定。
本轮Windows开发候选已本机交付，但不是最终0.9.0/main/tag正式发行payload；生产部署、发布和发布后安装同步尚未执行。
上一轮候选缓存维护回收逻辑空间 88.44 GiB，保留 release 后剩余2.58 GiB，当时40 GiB 预算达成。本轮f701单次formal维护仅处理候选自身 `project-target`：原192465560921字节，回收188034813885字节（约175.12GiB），剩4430747036字节（约4.13GiB），40GiB预算达成；保留release、SDK、dist、固定安装与数据。证据为 `delivery/runs/f70156e0a54dddb812584d24564f2601fe6e4ebe/final-cache-maintenance.json`；未扫描/清理tagless或未登记外部缓存，不宣称全机达标。
当时新登记的私有 MG 缓存5.73 GiB因缺真实 Cargo `CACHEDIR.TAG` 被清理工具拒绝，已保留，未伪造标记；
该历史任务两处缓存合计约8.31 GiB。清理工具的原始成功/失败记录均保留。
原后台任务曾报告外部未登记缓存约 140.52 GiB / 80.97 GiB，回收 0，40 GiB 预算未达成；
不自动清理未登记历史缓存，不将该限制隐藏为清理成功。

## 6. 七包、固定安装与完成条件

范围和版本冻结后先按 [版本规范](Rust_Versioning.md) 及 [发布管线](Rust_Release_Pipeline.md)
预检，再以最终 main/tag 身份在匹配原生主机构建各 payload 一次、验收一次。
目前 Cargo 与锁文件已冻结为 0.9.0，尚未创建对应标签。版本冻结前的Windows客户端 `f70156e` 开发候选已按实际
Cargo `0.8.7` 构建、验收并完成固定安装；核心GUI补验因工具阻塞未完成，保留release门禁。最终0.9.0/main/tag
身份及七包发行仍待冻结和验收。

| 公开归档（`<tag>` 为最终批准标签） | 验收与更新边界 |
| --- | --- |
| `AudioLibExplorer-<tag>-win-x64-portable.zip` | 实际 ASIO 配置、portable 验收、固定安装 |
| `AudioLibExplorer-<tag>-macos-arm64.zip` | `package-macos-client.ps1 -ValidateRuntime` 合并结构/签名/原生启动退出；ad-hoc不等于Apple公证，手动更新 |
| `AudioLibServer-<tag>-win-x64.zip` | server与离线migrate双身份、隔离health及8→9→8；首次手动引导；显式构建acceptance所需benchmark helper |
| `AudioLibServer-<tag>-macos-arm64.tar.gz` | 两个工具的原生启动、迁移/重开及包身份；首次手动引导，不自动部署生产 |
| `AudioLibServer-<tag>-linux-x64.tar.gz` | 两个production工具的匹配环境/文件系统与8→9→8验收；首次手动引导，仿真须明确记录 |
| `AudioLibServer-<tag>-docker-linux-amd64.tar.gz` | 消费已验收Linux双二进制输入；实际离线load/run、UID/只读/health/生产保护及两hash校验 |
| `AudioLibServer-<tag>-docker-linux-arm64.tar.gz` | 同上；独立 Linux arm64 原生包是内部输入，不列公开资产 |

各归档附 `.sha256`，合计十四项资产。Docker 通过替换镜像升级，禁止在容器内自动换二进制；
Mac 客户端按既有手动流程更新。发布核验只核远端元数据/摘要，不能替代本机迁移和包验收。

Windows 必须按 [仓库固定安装流程](../AGENTS.md#mandatory-local-application-delivery-and-release-installation)
将已通过 portable 验收的完整包通过**主工作区** `tools/install-local-release.ps1 -Source <候选portable>`
安装至该主工作区唯一固定 `local_release/`；候选工作树不另建日常安装。正常关闭并排空写入/扫描、
只复制 package-owned 文件、不清空目录，核对所有文件 SHA256、CLI 版本及一次显式隔离数据目录启动。
保留日常库、settings、日志、缓存和用户原环境；beta 独立安装不能替代固定安装。
若以后获准发布，再将固定安装与实际发布资产摘要闭环核验，分别报告远端发布和本地同步结果。
交付成功后执行一次有归属证据的缓存预算维护，保留 release 输出与诊断要求。
