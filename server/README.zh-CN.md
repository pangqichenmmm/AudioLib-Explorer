# audiolib-server

[English](README.md)

一个小型 HTTP 服务，把一个 AudioLib 音效库（SQLite 数据库 + 配置的根目录下的音频文件）通过 `/api/v1` 提供给远程 AudioLib 客户端。支持库快照与增量变更、音频流（Range 请求）、低带宽试听、波形峰值、元数据写入、远程扫描和仅局域网可用的管理页。服务端不会修改音频文件。

- **API 规范：** [doc/api/README.md](../doc/api/README.md)（目前为中文）。
- **部署、升级与回滚：** [deploy/README.md](deploy/README.md)（含 systemd、launchd、Docker 示例，英文）。
- **许可证：** 见仓库根目录的 LICENSE（See LICENSE at repo root）。

## 功能

- 每次启动在后台检查 GitHub Release；管理页提示新版本，确认后下载、校验、安全关闭并自动部署重启，见[自动更新](deploy/README.md#automatic-release-updates)。
- 库快照（`/library/snapshot`，可选 zstd）和基于 revision 的变更轮询。
- `/audio/{id}/stream`：单段 `Range`、`ETag`、`If-Range`；`?download=1` 用于下载。
- `/audio/{id}/preview` 低带宽试听：WAV 在进程内做 PCM 降采样（`codec=pcm`），可选 ffmpeg FLAC 转码（`codec=flac`）。
- `/audio/{id}/waveform`，带磁盘 LRU 缓存。
- `/albums/{id}/artwork` 按共享专辑返回内嵌 JPEG/PNG 封面（可选 `?side=`），以 `album_entity_artwork` 能力、库身份和专辑 namespace 绑定。兼容 `/audio/{id}/artwork` 保留原语义；图片不写入数据库，缓存在库目录旁的 `artwork/`。schema 8 升级须配套更新客户端，并保留原库回退备份；见[共享专辑与存储](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Shared_Albums.md)。
- 元数据写入（播放历史只在客户端本地记录；`POST /audio/{id}/plays` 已弃用为空操作，`GET /history/plays` 总是空）、远程扫描（开始/暂停/继续/取消）、记录与文件夹移除（可用 `manage_mode` 限制）。
- Token 鉴权三种模式（`off`、`external`、`all`），token 在管理页生成（只保存哈希）或来自环境变量，可选原生 TLS 监听。
- 内置管理页（`/admin`，仅局域网）：公网地址、鉴权模式、token、库根目录。

## 下载预编译服务端

[GitHub Releases](https://github.com/Freedragonlancer/AudioLib-Explorer/releases) 提供 Mac arm64 服务端归档；从 0.8.4 起增加 Linux x64 原生包和 Docker amd64/arm64 离线镜像，均附 `.sha256`。校验后再解压原生包或导入镜像。Linux 包以 Debian 12 为 GNU libc 构建基线；原生程序位于归档根目录，配置示例位于 `server/audiolib-server/`。Windows 和独立 Linux arm64 服务端从源码构建。升级保留原资料库、根目录、配置和凭据；Docker 保留数据卷并替换镜像，不在容器内更新二进制。详见 [Linux 与 Docker 交付](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Linux_Docker_Release.md) 和[部署指南](deploy/README.md)。

## 快速开始：从源码构建

仓库根目录的 `rust-toolchain.toml` 与 `.cargo/config.toml` 默认是 **Windows** 目标，请始终显式传入 `--target`。服务端构建 `audiolib-audio` 时不启用 `playback` feature，不需要音频设备库。

**macOS（Apple 芯片）**（`server/rust-toolchain.toml` 固定 1.97.1）：

```sh
cargo build -p audiolib-server --release --target aarch64-apple-darwin
# 产物：target/aarch64-apple-darwin/release/audiolib-server
```

**Linux（Debian/Ubuntu）**：

```sh
sudo apt-get install -y build-essential pkg-config clang libclang-dev
export RUSTUP_TOOLCHAIN=1.97.1          # 覆盖仓库的 Windows 工具链文件
rustup toolchain install 1.97.1
cargo build -p audiolib-server --release --locked --target x86_64-unknown-linux-gnu
#   ARM64 用 --target aarch64-unknown-linux-gnu
cargo test  -p audiolib-server --locked --target x86_64-unknown-linux-gnu   # 可选
```

已验证：Linux 构建与服务端测试在 aarch64 容器（`rust:1.97.1-bookworm`）中通过。已在 arm64（OrbStack）上自提交 d379894 用原版 Dockerfile 验证：镜像可构建（约 511 MB），容器可启动并在 8790 端口提供服务。

`GET /api/v1/health` 的 `commit` 是构建时嵌入的 git 短哈希；无 `.git` 的源码包构建时为 `unknown`。

## 配置

把 [`audiolib-server/config.example.toml`](audiolib-server/config.example.toml) 复制到仓库之外再编辑，每个选项在文件中都有注释。库数据库必须已存在，服务端不会创建。相对路径相对于配置文件所在目录。

鉴权由 `auth_mode` 决定（API 文档 §2.2）：

- `external`（默认）：使用可信 Host 的局域网直连请求不需要 token；经反向代理（`X-Forwarded-For`、`Forwarded`、`X-Real-IP`、`X-Forwarded-Proto`/`-Host`）或来自非私有网段的请求需要 `Authorization: Bearer <token>`。
- `off`：任何接口都不需要 token。
- `all`（或旧的 `auth = true`）：除 `/health` 外所有接口都需要 token。

有效 token：环境变量 token（`token_env` 指定的变量，默认 `AUDIOLIB_SERVER_TOKEN`，至少 32 个随机可见 ASCII 字符，不会写入日志）和管理页生成的 token。**安全建议：** 任何可从公网访问的部署都应使用带 TLS 的反向代理并要求 token；不要直接暴露明文 HTTP 端口，也不要使用 `auth_mode = "off"`。`external` 模式下局域网客户端无需 token 即被信任。

默认 `manage_mode = "lan"`，外部 token 不能浏览目录、添加根或扫描；显式配置 `all` 才授予完整管理权限。升级前请替换不足 32 字符的环境令牌，已生成的 `al_` 令牌仍有效。

Docker Desktop、rootless 容器、SNAT 或隐藏真实来源的反代应设置 `trust_lan_peers = false` 与 `auth_mode = "all"`；此时无密码管理页关闭，通过 TOML 管理。容器示例已采用此配置，端口只绑定宿主机回环。免令牌 API 同时校验 Host，失败鉴权按实际 socket IP 每分钟最多 60 次。

原生 TLS 每分钟检查续期文件，成功后只替换新握手的证书，失败时保留旧证书并显示原因。每个监听器最多 1024 连接（含握手）；波形整包复用缓存，最多 8 个下载，每次发送最长 15 分钟。

## 运行

```sh
audiolib-server --config /etc/audiolib/config.toml
# 使用环境变量 token：
AUDIOLIB_SERVER_TOKEN='<至少 32 个随机可见 ASCII 字符>' audiolib-server --config /etc/audiolib/config.toml
# 或：AUDIOLIB_SERVER_CONFIG=/etc/audiolib/config.toml audiolib-server
```

示例路径：`/srv/audiolib/music`（音频）、`/var/lib/audiolib`（库与数据）、`/opt/audiolib`（程序）、`/usr/local/var/audiolib`（macOS）。

运行行为：

- 连接必须在 10 秒内发完请求头；非流式接口 30 秒超时（408 `TIMEOUT`）；收到 SIGTERM/Ctrl+C 后，进行中的请求最多再等 10 秒。
- 先监听后打开库：端口立即可用，库在后台线程打开。此期间 `GET /api/v1/health` 返回 `"status":"starting"`，需鉴权的接口返回 503 `UNAVAILABLE`（`Retry-After: 5`）。打开失败（例如 macOS 可移动卷权限提示被拒绝）时 `/health` 返回 `"status":"error"`，并以退避方式重试（5 秒起翻倍，最长 300 秒）而不是退出。绑定、配置和 token 错误仍会立即退出。
- ffmpeg 是可选的（仅 `preview?codec=flac` 使用）；缺失时该编码返回 503 `TRANSCODER_UNAVAILABLE`，服务继续运行。服务端不捆绑 ffmpeg。

## 首次使用（管理页）

管理页没有密码，只响应**局域网直连**请求（私网对端地址、无转发头、局域网 `Host`，见 `src/admin.rs` 的 guard）。`manage_mode` 不控制管理页，只限制客户端 API 的扫描与移除操作。

1. 启动服务后，在同一局域网的电脑上打开 `http://192.0.2.10:8790/admin`。
2. **令牌：** 填名称后点「生成令牌」。令牌（`al_` + 64 位十六进制）明文只显示一次，服务端只在库旁的 `tokens.json` 保存 SHA-256 哈希，可随时撤销。也可用环境变量令牌（`AUDIOLIB_SERVER_TOKEN`）。是否需要令牌由 `auth_mode` 决定（管理页也可修改）。
3. **库根目录：** 只读浏览服务器磁盘并添加目录——本地盘或已挂载的网络共享（SMB，如 macOS 的 `/Volumes/...`、Linux 的 `/mnt/...`；需先由系统挂载，服务端不负责挂载）。立即生效无需重启，保存在 `admin-settings.json`。
4. **扫描：** 在管理页对根目录开始扫描并查看进度（`GET /api/v1/scans/current` 返回同样状态）。
5. 可选：设置公网地址（`/health` 的 `public_url`）和原生 TLS。

## Windows 客户端连接

客户端打开「音效库管理」（Ctrl+D）→「＋ 添加服务器」：

- **地址：** 填服务器根地址，如局域网 `http://192.0.2.10:8790`，外网 `https://audiolib.example.com`（可带路径前缀，如 `https://audiolib.example.com/audiolib`）。客户端自动追加 `/api/v1`；粘贴带 `/api/v1` 的地址也可以，不会重复（`crates/audiolib-core/src/server_url.rs`）。
- **访问令牌：** `auth_mode = "external"` 时局域网可留空；外网或 `auth_mode = "all"` 时必填。按配置用 Windows DPAPI 加密保存。
- 保存前先做连接测试（具体按钮以客户端界面为准）。
- **试听时转码：** 局域网不勾（直接流式原文件）；外网或低带宽勾选（走 `/audio/{id}/preview`；FLAC 需服务器装 ffmpeg，否则只有 WAV PCM 降采样）。

## 外网访问

不要直接暴露 8790 端口。用带 HTTPS 的反向代理（如 `https://audiolib.example.com` → `http://127.0.0.1:8790`），保持令牌必需，并且**绝不要把 `/admin` 暴露到公网**：反代只转发 `/api/v1/`，或屏蔽 `/admin`（`location /admin { return 403; }`）。反代必须发送转发头，使请求被识别为外网。完整清单与 nginx/Caddy 示例见 API 文档 §2.4 和 [deploy/README.md](deploy/README.md#reverse-proxy-checklist)。

## 常见问题

- **大库首次快照慢：** 百万文件约需 30–60 秒准备；超过 `snapshot_wait_secs`（默认 90）返回 503 `BUSY` + `Retry-After`，后台继续准备，客户端会重试。
- **409 `BUSY`**（如扫描中移除根目录）或 **503 `BUSY`**（流/试听并发已满）：按 `Retry-After` 稍后重试。
- **启动后 503 `UNAVAILABLE`：** 库仍在打开（`/health` 为 `starting`）。v3/v4 → v5 升级在打开时做 schema 迁移与提取标签回填（2026-10-11 匿名实测：大库副本约 198 秒，较小库约 7 秒；记录数及库容量不公开）。全文索引**不会**自动重建，需之后手动运行 CLI：`AudioLibCli db rebuild-search`（同库另约 513 秒）；该步骤会留下一份与库差不多大的回滚备份，升级前请预留足够磁盘空间。见 [deploy/README.md](deploy/README.md#search-index-backfill-and-fts-rebuild-db-rebuild-search)。
- **磁盘空间：** 快照在 `snapshot_dir`（默认库文件旁的 `snapshots/`），波形缓存在 `waveform_cache_dir`（受 `waveform_cache_mb` 限制），备份与回滚副本在库旁。

## 升级、回滚与备份

见 [deploy/README.md](deploy/README.md)（升级手册、`redeploy-server.sh` 自动回滚、备份与健康检查）。

## API 概览

`/health`、`/library`、`/library/snapshot`、`/roots`、`POST /roots/{name}/check`、`/audio/{id}`、`/audio/{id}/waveform`、`/audio/{id}/preview`、`/audio/{id}/stream`、`/audio/{id}/artwork`、元数据写入（`PATCH /audio/{id}/metadata`、`POST /audio/metadata/batch`、`POST /audio/{id}/plays`）、远程扫描（`POST /scans`、`GET /scans/current`、`GET /scans/{id}`、`POST /scans/{id}/pause|resume|cancel`）、历史（`GET /history/scans|plays`）、移除（`POST /audio/remove`、`DELETE /folders/{id}`）和 `GET /capabilities`。详情、错误码和示例见 [doc/api/README.md](../doc/api/README.md)。

库根目录可在仅局域网可用的管理页（`http://<局域网地址>:8790/admin`）管理：只读的服务器磁盘浏览、添加/移除根目录（移除只让库忘记记录，不触碰音频文件；扫描期间返回 409 `BUSY`）和扫描控制。修改即时生效，保存在库文件旁的 `admin-settings.json`（其中的 `roots` 此后取代 `config.toml` 的 `[[roots]]`）。`/admin/api/*` 不属于客户端 API。

## 部署

见 [deploy/README.md](deploy/README.md)。示例：`deploy/systemd/`、`deploy/launchd/`、`deploy/docker/`；可选的 macOS 专用启动器在 [launcher/](launcher/README.md)。

Docker 说明：

- 必须挂载一个包含 `/data/library.db` 的卷，或者先用客户端建好库；否则 `/api/v1/health` 会一直返回 `status:"error"`（The library could not be opened; the server keeps retrying）。
- Docker HEALTHCHECK 同时要求 HTTP 成功与 JSON `status:"ok"`；开库失败或启动中均为 unhealthy。
- 只有 `/data` 是卷（library.db、快照、缓存、tokens.json）。`/config` 不是卷：镜像自带 `/config/config.toml`，可选地用只读 bind mount 覆盖（`-v ./my-config.toml:/config/config.toml:ro`）。
- 镜像内置配置使用 `auth_mode = "all"`。直接 `docker run` 时必须传 `-e AUDIOLIB_SERVER_TOKEN=...`，否则服务能启动，但所有请求都会返回 401；compose 文件用 `${AUDIOLIB_SERVER_TOKEN:?}` 强制要求填写。

## 许可证

见仓库根目录的 LICENSE（See LICENSE at repo root）。
