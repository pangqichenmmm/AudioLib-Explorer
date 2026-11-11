# Schema 9 服务端目录迁移

从完整 schema 8 库升级到 `ale-s9-cld1-t3-m2-p1-a1-sd1`。切换事务按 MG-18
发布 `search_path: 1`、`vorbis_tags: 1`、`vendor_metadata: 1`。本迁移交付目录索引、
格式与 feature 标记。Vorbis 解码和厂商导入仍需各自的功能验收。

```powershell
cargo build --locked -p audiolib-db --bin audiolib-migrate
audiolib-migrate status --db ./library.db
audiolib-migrate start --db ./library.db
audiolib-migrate pause --db ./library.db
audiolib-migrate resume --db ./library.db
audiolib-migrate cancel --db ./library.db
audiolib-migrate rollback --db ./library.db
```

`startup` 与 `start` 相同，但尊重 `AUDIOLIB_DEFER_SCHEMA9=1`。服务端以管理员
显式启动迁移；普通桌面数据库打开仍以 schema 8 为写入目标。迁移后的服务端可重新打开
schema 9。命令不会创建缺失的库，也不会接管无关 SQLite 文件。

命令最后一行与 `/admin/api/migration/status`、
`POST /admin/api/migration/{start,pause,resume,cancel,rollback}` 返回相同的
[JSON 报告](schemas/migration-report.schema.json)。HTTP start/resume 在后台执行，
客户端轮询 status；其余控制操作等待结果。管理接口沿用 LAN、`X-AudioLib-Admin: 1`、同源校验，额外检查
`manage_mode`。备份和首批校准期间锁冲突返回 `locked`。首批提交后进入 running，
此后重入返回 `already_running`，CLI 退出码 2，HTTP 409。CLI 和 HTTP 共用判断。

本机临时验收服务使用 `auth_mode=off`、`manage_mode=all` 和回环监听地址。
原生客户端可以访问 `/admin/api/migration/*`，条件为连接两端均是回环地址、
没有转发头、Origin 或 Sec-Fetch-Site，修改请求使用 `application/json`。
Host 仍须通过管理入口检查。`manage_mode=off|lan` 仍拒绝回环管理操作。
默认 external 和 all 认证模式继续拒绝回环访问管理入口，其他管理路由继续使用既有守卫。

读接口遵循 [HTTP 协议](schemas/migration-http.schema.json)：

- 状态非 idle/done 时发出 `x-audiolib-migration`；旧长名称只作客户端读取别名。
- 搜索 scope 为 `main|all|path`，默认 main；GET `/api/v1/search?q=...&scope=...`
  与既有 POST 搜索共用 DataProvider。
- 路径索引未发布时 path 返回 503 `migration_in_progress`、`Retry-After: 30`；
  all 返回主表结果并带 `x-audiolib-path-search: unavailable`。
- 回填期间，相同的有效 main 查询按 DataProvider 搜索修订复用一个至多 4 MiB 的
  完整 JSON 响应，含该次查询的真实诊断字段。all 回退与 main 可共享该响应。
  本进程写入、外部提交及恢复会使缓存失效，期间发生提交的查询结果不进入缓存。
- switching 时写接口返回 503 `migration_switching`、`Retry-After: 30`；读接口继续服务。

迁移预检只读权限和额外可用空间（至少库大小的 1.3 倍）。SQLite 在线备份写入同目录
唯一的 `audiolib-rollback-mig9-*.db`，验证完整性、外键及库身份后保留，绝不自动删除。
不执行 VACUUM。

每批默认上限为 5,000 个目录，小库按剩余目录数的四分之一向上取整起步，
至少一行，以便在回填期间采样和调整写锁时长。批次变慢后缩小下一批。目录行与
`v9_checkpoint` 同事务提交。仅 `audio_dir_search` 的本段目录名及其查询词进行
NFC、全角 ASCII/空格转换；根目录不进入词项。`audio_search`、原始路径、Metadata
及注释不被迁移改写。回填期间捕获目录增删改；切换事务重放目录变更并生成
`audio_dir_runs`，原子发布 `search_path_rev=1`、`source_search_rev=3`、格式 ID 和
`user_version=9` 及三项 feature 标记。故障恢复后仍以事务提交结果为准。
旧目录版 schema 9 可继续读取，只有一项 Metadata feature 的部分发布状态会被拒绝。

迁移使用持久的 `<db>.migration9.lock` OS 锁，文件写入 owner PID，进程退出自动释放。
不能复用已有 `.upgrade.lock`：普通搜索读者已经用它协调旧式离线重建。锁文件不删除，
避免锁 inode 被替换的竞态。`v9_control` 接收跨进程 pause/cancel，执行器在批次边界检查。
控制请求遇到准备阶段或忙锁可重试。SQLITE_FULL 保留断点并报告 paused/disk_full。
持久报告绑定库的 album namespace；异常退出后无 owner 的运行状态按 paused 读取。

取消删除未发布的路径派生对象，保留备份及 schema 8 业务数据。回滚须验证保留备份，
持有扫描和独占管理 lease，然后事务撤销路径对象及格式标记。本阶段只修改派生对象，
因此采用逆向事务回滚，保留备份之后的注释和扫描数据，避免覆盖新写入。

故障注入只在 `--features test-hooks` 构建中存在：
`AUDIOLIB_MIGRATION_FAULT`、`FAKE_FREE_BYTES`、`PAUSE_AFTER_BATCHES`、`BATCH_ROWS`、
`SLOW_BATCH`、`HOLD_SWITCH_MS`（后五项均带 `AUDIOLIB_MIGRATION_` 前缀），以及
`--sql-trace`。默认构建不解释这些变量或选项。测试的 abort 以无析构进程退出模拟崩溃，
不生成平台 crash dump。

## 验证范围与既有 harness 差异

目录契约回归位于 `crates/audiolib-db/tests/migration9.rs`；权限、状态头、scope 回退及
切换拒写回归位于 `server/audiolib-server/tests/migration9.rs`。

PR #211 的原始断言和 schema 文件保持不变，结果须逐项保留：

- MG01/MG10/MG17 读取逐文件 `audio_path_search`，与 t3 的目录表不一致。
- MG05 在首批校准时检验 `locked`，MG14 在 running 时检验 `already_running`。
  阶段边界以首批 checkpoint 的提交为准，稳定 OS 锁在两个阶段均持续持有。
- 小目录树现在可以触发 MG11 的慢批缩减和 MG13 的第二批 SQLITE_FULL。
  MG13 恢复后的终态仍会读取旧 `audio_path_search`，须按实际失败位置记录。
- MG18 的三项 feature 标记与格式、搜索修订和 schema 版本在同一事务提交，
  rollback 事务一并撤销。Metadata 功能的完整验收另见 Vorbis 和厂商导入用例。
- HTTP 辅助驱动使用原生 `/api/v1` URI、PATCH 注释接口、管理头、客户端兼容目录，
  并在连接 fixture 时读取和固定服务端 library_id。`smv2_mg.rs` 的断言保持原样。
  全量运行可指定 `AUDIOLIB_SMV2_RESET_SERVER_CMD`，在每条 HTTP 用例初始化前
  同步重启独占的临时服务并提供全新 schema 8 库，URL 和 ADMIN_DB 路径保持稳定。
  该命令应只管理测试者创建的临时目录，并等待旧实例正常退出。
  HOLD_SWITCH_MS 由需要观察切换窗口的用例显式传入，其他用例的子进程清除此继承值，
  避免人工保持时间污染 MG11 写锁时长。
  未指定时仍支持逐例独立启动临时服务。
- 临时 HTTP 库须包含可播放且带封面的真实合成 WAV，以及足以观察慢批控制的目录。
  `/api/vendor-import` 及子路径在切换期执行已授权写入的 503 屏障，其他状态由正常路由处理。


验收必须区分契约差异、环境问题及实现错误，不能增加旧路径表或解除权限检查来满足
断言。Windows bench 基线问题应独立记录，不修改 `search-b-bench.rs` 来绕过验收。
大库耗时、跨平台文件系统及真实设备性能需要各自实测；
合成 fixture 的结果不代表 **P95 latency <500ms for 1M+ records** 的性能验收。
