# Codex 交接：Search Metadata v2（2026-11-03）

本阶段节点：**冷测闸门通过 + #211 合入 feat**，已达成。后续开发交给 Codex，各 bot 只在 PR 上审查。

## 0. 接手第一步：全仓检查

#219 等合入时 CI 只跑了 fmt，全仓 clippy 和 test 没跑过。接手后先在 feat `f765eca`（#220 合入后的 tip）上跑：

```
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

有失败先修，再开新任务。

## 1. 分支与 tip

| 项 | 值 |
|---|---|
| 工作分支 | `feat/search-metadata-v2`，tip `f765eca`（#220） |
| 已合入（按顺序） | #214、#215、#216、#213、#208、#217、#219（`6980bac`）、#211（`b24eea5`）、#220（`f765eca`） |
| `main` / `beta` | 本阶段未动 |
| 未合 | #218（`feat/search-v2-client-ux2`，tip `1988eef`，WIP）、#209（设计稿 PR） |

所有新 PR 的目标分支都是 `feat/search-metadata-v2`，squash 合入。

## 2. 约定

- 格式 ID：`ale-s9-cld1-t3-m2-p1-a1-sd1`（结尾是 sd1，不是 sp1）。
- 迁移响应头：正式名 `x-audiolib-migration`；旧名 `x-audiolib-schema-migration` 只作只读别名，两者同时出现以正式名为准。
- 迁移范围：`main|all|path`。
- 迁移进行中，路径搜索返回 503 `migration_in_progress`，带 `Retry-After: 30`；切换阶段的写操作返回 503 `migration_switching`。
- 性能闸门：每条查询 5 轮，取各轮 p95 的中位数，须 ≤ s8 × 1.2。

## 3. 交给 Codex 的任务

1. **schema 9 迁移（服务端）**：`audiolib-migrate`、test-hooks、`/admin/api/migration/*`。已在做；PR 进 feat，由服务端 bot 审接口和迁移逻辑。合入后跑第 4 节的 `smv2_mg`。
2. **客户端 UI（接 #218）**：§7.1 schema 9 迁移界面、厂商 CSV/XLS(X) 导入界面、§7.6「点击刷新」文案。#218 是可用 WIP，测试未跑完；具体缺口以 #218 的 PR 描述为准。
3. **冷测排查**：若后续冷测有条目超闸门，由 Codex 排查原因。

## 4. `smv2_mg` 重跑方法（迁移合入 feat 后）

- 起一个本地临时服务端，`auth_mode=off`，用临时库，**不连生产实例**。
- 环境变量：`AUDIOLIB_SMV2_ADMIN_URL` 指向该服务端，`AUDIOLIB_SMV2_ADMIN_DB` 指向临时库，`AUDIOLIB_SMV2_MIGRATE_CMD` 指向 `audiolib-migrate`；需要管理 token 时用 `AUDIOLIB_SMV2_ADMIN_TOKEN`，不要写进代码。
- 运行：`cargo test -p audiolib-db --test smv2_mg -- --ignored --test-threads=1`，预期 24 条全过。

## 5. 冷测（压测）

- 程序：`crates/audiolib-db/src/bin/search-b-bench.rs`，脚本 `tools/bench/cold.sh`。
- 开关 `BENCH_PREPARE_OUTSIDE_TIMER=1`：s8 和 s9 都开，只把全文检索语句的准备移到计时外，每个样本前仍执行 purge。
- 挂载前缀从环境变量 `MOUNT_PREFIX` 读，不要写死本地路径。
- 本阶段结果：20/20 通过，s9/s8 倍数 0.807–1.079。

## 6. Windows 开发注意

- 仓库若在网络共享盘上，git 会因目录归属检查拒绝运行：每条命令前加 `-c safe.directory=<路径>`，不要改全局 git 配置。
- `gh` 一律带 `-R Freedragonlancer/AudioLib-Explorer`，不依赖本地仓库。
- `CARGO_TARGET_DIR` 放本地盘（如 C:），放共享盘上链接会失败。
- 工具链固定用 MSVC，不用 GNU。

## 7. 禁令

- #97 不合。
- 生产实例不动（不升级、不重建索引）。
- feat→beta、合 main、发版都要 Paul 点头。
- 不提交内网 IP、私有域名、本地路径或密钥。
- 不 force push `main`、`beta`、`feat/*`。
