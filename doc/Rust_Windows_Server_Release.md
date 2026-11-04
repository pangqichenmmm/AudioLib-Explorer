# Windows 服务端原生发布

0.8.5 的 Windows x64 服务端交付契约；发布状态以对应 GitHub Release 的实际资产和验证结果为准。API、schema 和既有 Release 保持原有契约。

## 范围与首个纳入版本

- 首个纳入版本：**0.8.5**（含其 `-beta.N` / `-rc.N` 标签）。门槛为 `tools/verify-release-assets.ps1` 中的常量 `$WindowsServerFirstVersion`。
- 0.8.4 及更早 Release 保持原资产集合（0.8.4 为六个归档、十二个资产），不追加、不改名。
- 0.8.5 起每个 Release 为七个归档、十四个资产：在 0.8.4 的六个归档外增加 Windows x64 服务端。
- 源码构建继续可用；之前版本的 Windows 服务端仍需从源码构建。

## 资产与包形态

| 项 | 约定 |
| --- | --- |
| 归档 | `AudioLibServer-vX.Y.Z-win-x64.zip` |
| 校验 | `AudioLibServer-vX.Y.Z-win-x64.zip.sha256`（`<SHA256> <归档名>`） |
| 根目录 | `audiolib-server.exe`、`update-channel.txt`、`update-version.txt` |
| 公共文件 | 与 Mac/Linux 相同的 `tools/server-files.json` 清单及 `licenses/` |
| 签名 | 与其他资产一致：SHA256 + GitHub HTTPS；不做 Authenticode（后续另议） |

命名依据：`tools/package-server.ps1`（`win-x64`、`zip`）和 `crates/audiolib-update` 的 `asset_names_for_platform(Product::Server, WindowsX64)` 已使用此名称，与 `AudioLibServer-<tag>-macos-arm64.tar.gz`、`AudioLibServer-<tag>-linux-x64.tar.gz` 同一规则。

## 构建、打包与验收

```powershell
$env:RUSTUP_TOOLCHAIN = '1.97.1-x86_64-pc-windows-msvc'
pwsh tools/build-server-release.ps1 -Target x86_64-pc-windows-msvc
pwsh tools/package-server.ps1 -Tag vX.Y.Z -Target x86_64-pc-windows-msvc
# 仅本次验收需要的 fixture helper；普通客户端/服务端发布构建不生成它。
# 与后面的验收使用相同 RUSTUP_TOOLCHAIN / CARGO_TARGET_DIR 和 target。
cargo build -p audiolib-db --bin search-benchmark --release --locked --target x86_64-pc-windows-msvc
pwsh tools/validate-server-package.ps1 -Tag vX.Y.Z -Target x86_64-pc-windows-msvc
```

- `build-server-release.ps1` 的 Windows 分支只构建 `audiolib-server`（`cargo build -p audiolib-server --release --locked --no-default-features`），保留工作区的静态 MSVC CRT 与路径重映射。
- **默认不传 `-Asio`**：服务端不打开音频设备，不依赖 ASIO；`-Asio` 仅保留给历史打包兼容，不用于正式 Windows 服务端包。
- 打包与验收必须在 Windows x64 上执行；验收只使用临时库与临时端口，不连接生产服务。
- 正式包须从最终 main/tag 提交构建，与其他平台相同。

## 更新策略

- 桌面客户端更新器不管理服务端（`Product::Desktop` 与 `Product::Server` 分开）。
- 服务端自更新沿用管理页面（`/admin/api/update*`），按运行平台选择 Windows ZIP；Windows 由独立 helper 等待原进程退出后替换。
- 程序目录须对运行账户可写，配置与运行数据放在程序目录之外；更新保留原启动参数及工作目录。
- **不修改 `tools/server-files.json`**：包内公共文件须继续满足 `tools/update-compatible-files.json` 冻结的 v0.7.8 基线，避免旧更新器拒绝新包。Windows 服务安装样例等新增文件不进入首版包。
- 过渡：0.8.5 之前的源码安装若目录中有 `update-channel.txt` / `update-version.txt` 且布局为平铺包，可经管理页更新到 0.8.5；否则先手动解压 0.8.5 ZIP 到程序目录，保留数据与配置。

## 与 Mac/Linux 对齐的契约

- 资产命名 `AudioLibServer-<tag>-<platform>.<ext>` + 同名 `.sha256`。
- `audiolib-server --version` 输出 `audiolib-server X.Y.Z`；`GET /api/v1/health` 返回 `status`、`version`、`api_version`、`schema_version`、`commit`，`/health` 不需令牌。
- 程序目录只含发布包拥有的文件；资料库、`tokens.json`、快照、缓存、artwork、配置与日志都在程序目录之外，更新不触及。
- 配置项（`bind`、`library_db`、`auth_mode`、`manage_mode`、`trust_lan_peers`、`token_env`、`public_url` 等）与相对路径规则（相对配置文件目录）跨平台一致；服务端不创建空库。
- 能力（capabilities）、API `/api/v1/` 与 schema 兼容范围不因操作系统而不同。

## 配置与首次启动

已选定的推荐配置/数据目录为 `%ProgramData%\AudioLibServer`（2026-10-30 确认），与程序目录分离。程序不会自动选择或创建这个目录；必须通过 `--config <路径>` 或 `AUDIOLIB_SERVER_CONFIG` 显式指定配置。升级时保留已有配置和资料库路径。

校验 ZIP 与 `.sha256` 后解压到程序目录。在解压目录中首次复制配置示例，保留已有配置：

```powershell
$serverData = Join-Path $env:ProgramData 'AudioLibServer'
New-Item -ItemType Directory -Path $serverData -Force | Out-Null
$serverConfig = Join-Path $serverData 'config.toml'
if (!(Test-Path -LiteralPath $serverConfig)) {
    Copy-Item -LiteralPath .\server\audiolib-server\config.example.toml -Destination $serverConfig
}
```

编辑该配置，把 Unix 示例中的 `library_db` 和 `[[roots]].server_path` 换为已有的 Windows 资料库与真实音频目录，例如 `library_db = 'C:\ProgramData\AudioLibServer\library.db'` 和 `server_path = 'D:\Sounds'`。服务端不创建空库；需要新库时先使用客户端或 `AudioLibCli db create <路径>`。如需 FLAC 试听，单独安装 ffmpeg 并设置绝对路径，例如 `ffmpeg_path = 'C:\Tools\ffmpeg\bin\ffmpeg.exe'`；默认值为 macOS Homebrew 的 `/opt/homebrew/bin/ffmpeg`。缺少 ffmpeg 时仅 FLAC 试听返回 503 `TRANSCODER_UNAVAILABLE`，PCM 试听及直接流仍可用。

保存配置后启动：

```powershell
.\audiolib-server.exe --config $serverConfig
```

运行账户需要配置/数据目录写权限；使用自更新还需要程序目录写权限。ZIP 不注册 Windows 服务。

## 二期

- Windows 服务样例（SCM / 服务包装器）、启停与健康检查回滚脚本，对应 Unix 的 systemd/launchd 与 `redeploy-server.sh`。是否进入发布包须先评估 v0.7.8 更新器清单兼容性。
