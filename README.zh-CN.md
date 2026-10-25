# AudioLib Explorer — Maomao Edition

由 [Maomao（pangqichenmmm）](https://github.com/pangqichenmmm)维护，基于[官方 AudioLib Explorer 0.7.5](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.7.5)。本分支的 Windows 客户端保留 REAPER 参考频谱峰值配色，设置页色带与实际 Waveform 使用相同颜色。颜色按参考截图校准，频率分析仍沿用原有实现。

从[本仓库 Releases](https://github.com/pangqichenmmm/AudioLib-Explorer/releases)下载 Windows 便携包，通过 `Start-AudioLibExplorer.cmd` 启动。脚本关闭官方更新检查；后续升级会将本分支修改合并到新版源码后重新发布。程序目录可以同步，每台电脑的配置、凭据和缓存继续保存在本机。Release 中如附带 macOS 服务端，属于官方原始包，其来源与校验值会在发布说明中单独标明。

<img src="resources/icons/AudioLibExplorer.png" width="128" height="128" alt="AudioLib Explorer：音频素材库与 Waveform 图标">

[English](README.md)

AudioLib Explorer 是一款面向 Windows 的本地优先音效库管理软件，使用 Rust 编写，桌面界面基于 [Slint](https://slint.dev)。它把大型 WAV 音效库索引到 SQLite（FTS5）中，提供即时搜索，通过 WASAPI（可选 ASIO）试听，也可以浏览由自带的 `audiolib-server` 提供的远程音效库。

- **程序版本：** 0.7.5（工作区 `Cargo.toml`）
- **HTTP API：** `/api/v1/`（客户端与服务端）
- **数据库 schema：** 6（快照读取范围 3..=6，仍须验证实际格式）
- **可写库格式：** `ale-s6-cld1-t3-m2-p1`；此前 p0 库仅作为只读来源，经 owned 候选迁移后写入
- **许可证：** [GPL-3.0-only](LICENSE)

## 功能

- 最多 6 个独立搜索任务页签（外观设置可调整上限），分别保留目录、筛选和列表状态；个人搜索任务收藏按库分类保存，重启恢复打开任务。详见[搜索任务](doc/Rust_Search_Tasks.md)。
- 增量/完整文件夹扫描，提取专业 WAV Metadata（BWF `bext`、iXML、INFO、ID3、XMP，只读兼容 Soundminer SMED）。不会修改源 WAV 文件。
- 基于 SQLite FTS5 的搜索，支持中文分词（jieba-rs）、标签/分类筛选；文档化目标为 100 万以上记录 P95 < 500 ms。
- 流式解码播放、设备采样率重采样、波形与频谱视图、独立的音高/速率试听（SoundTouch），支持 WASAPI 与可选 ASIO 输出。
- 资料库管理：添加文件夹、修复被移动或改名的文件、批量标注、备份/恢复、完整性检查、JSONL/CSV 交换。
- 命令行工具 `AudioLibCli`（JSON/表格/CSV 输出、库管理、控制正在运行的桌面实例播放）。
- 服务器模式：`server/audiolib-server` 通过 `/api/v1/` 提供一个资料库（快照、Range 流式音频、预览、Waveform、令牌认证、仅局域网管理页）。客户端只读打开经过校验的快照；Metadata 和管理写入使用绑定库身份的 HTTP 请求。已测旧服务端 0.4.11 和 0.5.0 仍可读取。
- 中英文双语界面。

部署 schema v6 前先升级客户端，再在维护窗口迁移服务端。实际库修改必须携带有效现代兼容目录，明确支持服务端当前完整格式。旧播放路由继续返回空操作结果，但 HTTP 200 不保证旧客户端能反序列化响应。播放历史仅保存在各客户端本机。详见 [v6 契约](doc/api/Schema_V6_Iteration.md)。

## 构建

要求：Windows、MSVC 构建工具以及固定版本的 Rust 工具链（见 `rust-toolchain.toml`）。ASIO 构建还需要 LLVM（libclang），并使用固定且校验哈希的 Steinberg ASIO SDK（由 `tools/prepare-asio.ps1` 下载）。

```powershell
cargo build --release -p audiolib-desktop -p audiolib-cli

# 可选：ASIO 支持
. ./tools/prepare-asio.ps1
cargo build --release -p audiolib-desktop -p audiolib-cli --features audiolib-desktop/asio,audiolib-cli/asio

# 便携包（dist/latest/portable）
./tools/build-release.ps1
./tools/package-portable.ps1 -SkipBuild
```

服务端可在 Windows、Linux、macOS 上构建（不需要音频播放支持）。由于仓库的 `.cargo/config.toml` 默认目标是 Windows，需显式指定目标：

```sh
cargo build --release -p audiolib-server --locked --target "$(rustc -vV | sed -n 's/^host: //p')"
```

开发检查（以下以数据库片段搜索改动为例，按实际改动选择 package 和测试目标）：

```powershell
cargo fmt --all -- --check
cargo clippy -p audiolib-db --all-targets --locked -- -D warnings
cargo test -p audiolib-db --test search_fragments --locked
```

纯文档只需检查差异、链接和 `git diff --check`；UI 文案和布局改动检查编译及受影响界面。影响广泛时在本地运行 `./tools/check-local.ps1`，打发布标签前加 `-Asio` 完成本地发布矩阵；专项回归和性能测量按实际行为变化触发。GitHub 常规 CI 只检查格式。详见[验证范围](AGENTS.md#development-checks-by-change-scope)和[测试示例](doc/Testing_Strategy.md)。

## 使用

### 桌面客户端与 CLI

```powershell
cargo run -p audiolib-desktop
cargo run -p audiolib-cli -- scan "D:/Sounds" --full
cargo run -p audiolib-cli -- search "explosion" --format table
cargo run -p audiolib-cli -- stats
```

用户设置、日志、缓存和资料库位于 `%LOCALAPPDATA%/AudioLibExplorer/v1`；设置 `AUDIOLIB_DATA_DIR` 可使用隔离的数据目录。在“数据源”窗口选择服务器模式，填写服务器地址（例如 `https://audiolib.example.com:8443` 或 `http://server.example.lan:8790`），服务器要求时再填访问令牌。详见 [远程模式](doc/Rust_Remote_Mode.md) 与 [资料库管理](doc/Rust_Library_Management.md)。

### 服务端

```sh
cp server/audiolib-server/config.example.toml /etc/audiolib/config.toml   # 编辑根目录和数据库路径
audiolib-server --config /etc/audiolib/config.toml
```

**部署与使用指南：** [server/README.zh-CN.md](server/README.zh-CN.md)（构建、首次使用、客户端连接、外网访问）与 [server/deploy/README.md](server/deploy/README.md)（systemd/launchd/Docker、升级、备份）。配置、认证与部署（systemd、launchd、Docker 示例）见 [server/README.zh-CN.md](server/README.zh-CN.md)，API 契约见 [doc/api/README.md](doc/api/README.md)。面向公网时请放在带 TLS 的反向代理之后并启用令牌。

## 版本规范

程序 SemVer（工作区 `Cargo.toml`，客户端、CLI、服务端共用）、HTTP API 版本（`/api/v1/`）、数据库 schema（`SCHEMA_VERSION`，当前为 6）互相独立。允许的操作还须由兼容目录及完整 feature/format 描述判定；同为 schema 6 的 p0 只读，p1 才是当前可写目标。详见[迁移兼容契约](doc/api/Library_Migration_Compatibility.md)。schema 变化时程序版本至少升 MINOR。发布标签为 `vX.Y.Z` 或 `vX.Y.Z-rc.N`，必须与 `Cargo.toml` 一致，由 `tools/check-release.ps1` 检查。详见 [doc/Rust_Versioning.md](doc/Rust_Versioning.md)。

## 发布与 CI

客户端与服务端在每次正常启动后于后台检查 GitHub Release。发现新版后，在客户端提示或服务端管理页面确认，即可下载、校验，并在现有工作排空后部署。正式版和 beta 保留各自的更新渠道，资料库和偏好设置不属于更新包。渠道、打包元数据及部署方式详见[自动更新](doc/Rust_Auto_Update.md)。

当前开发分支为 `0.6.0-dev.1`，已合入发布的 0.5.0 主线。此分支的 schema-v6 改动尚未发布或部署，现有安装和工作库保持不变。

GitHub Actions 在相关 `main` / `codex/**` 推送及 Pull Request 时只运行一个 Linux 格式检查作业，纯文档和截图变更跳过。Rust 测试、Clippy、ASIO 矩阵、UI/服务端启动、回归和性能测量按范围在本地执行。发布包在本机构建、验证并上传；`.github/workflows/release.yml` 只保留手动触发，推送标签不会启动它。云端只核验实际包的允许文件、隐私、二进制/归档身份和校验和，运行时验证明确报告为 `not-run`。发布归档附 SHA-256 和公开构建报告。

## 文档

`doc/` 下的大部分文档目前为中文（如[版本规范](doc/Rust_Versioning.md)、[架构](doc/Rust_Architecture.md)、[验证记录](doc/Rust_Validation.md)），英文版将在后续补充。[文档索引](doc/INDEX.md)区分当前指南与历史设计文档。贡献者约定见 [AGENTS.md](AGENTS.md)。

## 许可证

AudioLib Explorer 是自由软件，采用 GNU 通用公共许可证第 3 版（仅 GPL-3.0）。见 [LICENSE](LICENSE)。第三方组件及其许可证见 [THIRD_PARTY.md](THIRD_PARTY.md)。
