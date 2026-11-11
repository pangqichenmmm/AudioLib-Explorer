# 封面功能的数据与兼容性审查（2026-10-27）

> 本记录描述生产部署前的代码与兼容性审查。后续授权的 Mac 升级及业务数据
> 一致性验证见[最新验证报告](Rust_Validation.md)。

本记录针对 `handoff/codex-20261027` 的 0.8.0 基线和后续封面接线。
审查以当前 Rust 源码、隔离合成库回归和现有 schema 6 镜像的只读验收为依据；
未升级或重启生产服务器，未扫描或改写源 WAV，也未复制、升级或全量哈希实际镜像。

## 实际数据库变化

- 本地可写库从 schema 6 升至 7：新增 `audio_files.album` 文本派生列和
  `idx_audio_visible_album` 索引。列值来自已有
  `metadata_json.wav_metadata.fields.library`，没有读取音频或把封面像素写入 SQLite。
- 受支持的完整 schema 6 远程镜像仍按原路径只读打开；单条 DTO 的 `album`
  从已有 Metadata 派生。显示选中音频封面不要求镜像升级或增加图片表。
- 本地增量扫描更新 `album` 时继续合并未知 Metadata 扩展并保留用户标注。
  封面解码在独立封面 worker 中进行，普通播放头读取和普通 Metadata 提取不解码图片。
- 原始 Metadata 是权威值，`album` 是可重建的索引辅助值。JSONL 导出包含 DTO 中的
  `album`；CSV 继续导出 `attributes` 中的原始库名。注释导入只更新用户标注，
  不把导出文件中的派生字段或 Metadata 覆盖进原库。
- 备份/恢复复制数据库，不包括封面缓存。恢复、迁移和本地扫描仍使用既有数据安全边界。
  已升级的本地 schema 7 库需要支持该格式的客户端，不能据此声称 0.7.8 可直接打开它。

## 0.8 播放调查中的 schema 6 假设

交接笔记提出：旧镜像可能因缺少 `album` 列被误复制、升级，从而看起来无法播放。
对本机现有约 10.6 GB 镜像的只读检查确认：schema 为 6，未增加 `album` 列，
tokenization revision 为 3，source-search/source-tags revision 均为 2，
search-index revision 为 1，四个必需搜索索引齐全。

使用当前 `SqliteProvider` 直接验收：

- `has_current_search_state` 返回 true。
- `open_read_only().database_path()` 等于原镜像路径，未创建 owned 临时副本。
- 一条实际 present 音频可由 `get_audio` 返回，`album` 与该条 Metadata 的派生值一致。
- 完整验收耗时 0.24 秒；源文件大小和修改时间保持不变。

该结果排除了**这份镜像**的误升级/整库复制假设，不证明所有旧或损坏镜像都能跳过升级，
也不代表实际音频流或物理音频输出已通过验收。播放链路证据另行记录。

## 已加强的回归

`crates/audiolib-db/src/album_tests.rs`：

- 完整 Plan B schema 6 fixture 断言保持原路径、无 owned 副本、版本仍为 6，
  同时支持单条读取、专辑筛选/计数和服务器注释补丁。补丁后 Metadata 扩展仍保留。
- schema 7 库经过备份/恢复、JSONL 和 CSV 注释导出/导入后，原始 Metadata、
  未知扩展、库名和用户备注保持一致。注释写入正常推进 DTO 的 `modified_at` 修订。
- 显式忽略的真实镜像验收由 `AUDIOLIB_TEST_V6_SNAPSHOT` 和
  `AUDIOLIB_TEST_AUDIO_ID` 指定输入；先确认搜索状态已就绪，再调用只读打开，
  防止把未就绪的大镜像送入复制路径。默认测试不访问用户目录。

验证命令：

```powershell
cargo test -p audiolib-db --lib album_tests --locked
cargo test -p audiolib-db --lib album_tests::existing_v6_mirror_opens_in_place_and_reads_audio --locked -- --ignored --exact
```

Windows / Rust 1.97.1 MSVC：原有六项专辑回归通过；新增备份/导出/导入/恢复回归通过；
真实 schema 6 镜像的显式只读验收通过。最终工作区检查和交付记录由总体验收补充。

## 性能边界

schema 6 没有 `album` 索引，`album_facets` 会对每条候选记录解析 Metadata 并分组。
在大镜像上反复构建专辑侧栏可能造成明显停顿。因此旧 schema 6 远程镜像应跳过此侧栏，
保留选中音频的按需封面；只在已有 schema 7 派生列/索引时启用远程专辑侧栏。
不能为了侧栏主动升级远程镜像，也不能把单条读取验收解释为全库专辑统计性能验收。

本轮未运行百万条基准。主要性能目标仍为 **P95 latency <500ms for 1M+ records**；
以上 0.24 秒只读验收不是该目标的测量结果。macOS/Linux 运行、真实设备音频输出
和网络盘冷启动覆盖由相应平台与播放验收单独说明。
