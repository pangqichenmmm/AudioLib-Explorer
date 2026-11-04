# Linux 与 Docker 服务端交付

程序 0.8.5 | API /api/v1/ | schema 8 | 快照兼容 3..=8

0.8.4 起，Release 增加以下归档，每个都附同名 `.sha256`；0.8.3 及更早版本的资产保持原样。

| 归档 | 用途 |
| --- | --- |
| `AudioLibServer-<tag>-linux-x64.tar.gz` | x86_64 GNU/Linux 原生服务端 |
| `AudioLibServer-<tag>-docker-linux-amd64.tar.gz` | Docker Linux amd64 镜像离线包 |
| `AudioLibServer-<tag>-docker-linux-arm64.tar.gz` | Docker Linux arm64 镜像离线包 |

Mac arm64 服务端及两个桌面客户端继续发布。独立 Linux arm64 包用于构建、验证 Docker 输入，不作为正式资产上传；Windows 服务端从 0.8.5（首个纳入版本）起提供预编译 ZIP，之前版本和源码构建仍可用，见 [Windows 服务端发布](Rust_Windows_Server_Release.md)。程序版本、API、schema 和 Metadata 格式沿用现有契约，不执行部署或自动迁移生产资料库。

## Linux 原生安装

下载 Linux x64 归档及校验文件，先校验再解压：

```sh
sha256sum -c AudioLibServer-v0.8.5-linux-x64.tar.gz.sha256
tar -xzf AudioLibServer-v0.8.5-linux-x64.tar.gz
./audiolib-server --version
./audiolib-server --config /etc/audiolib/config.toml
```

正式 Linux 包以 Debian 12 / GNU libc 为构建基线；实际 ELF 依赖、GNU libc 符号版本及验收架构以本版实测报告为准。不要把 GNU/Linux 包当作 Alpine/musl 包。配置示例位于 `server/audiolib-server/config.example.toml`，systemd 示例位于 `server/deploy/systemd/`。保持原有资料库、令牌、根目录和缓存配置；更新只替换包拥有的程序文件。Linux x64 管理页面可以使用已发布的匹配平台归档更新。

## Docker 离线导入

选择与宿主机匹配的归档。以下为 amd64；arm64 将文件名和镜像标签中的 `amd64` 换为 `arm64`：

```sh
sha256sum -c AudioLibServer-v0.8.5-docker-linux-amd64.tar.gz.sha256
docker load --input AudioLibServer-v0.8.5-docker-linux-amd64.tar.gz
docker image inspect audiolib-server:0.8.5-linux-amd64
```

这是 `docker save` 格式的压缩镜像归档；无需登录镜像仓库。Compose 文件见本版源码中的 [docker-compose.release.yml](../server/deploy/docker/docker-compose.release.yml)：

```sh
export AUDIOLIB_SERVER_IMAGE=audiolib-server:0.8.5-linux-amd64
export AUDIOLIB_SERVER_TOKEN='<至少 32 个可见 ASCII 字符的随机令牌>'
export AUDIOLIB_MUSIC_DIR=/srv/audiolib/music
docker compose -p audiolib-server -f server/deploy/docker/docker-compose.release.yml up -d
```

首次启动前需在 Compose 的数据卷中放入已有资料库的受支持、一致性备份，且 UID/GID `10001:10001` 可写。不要直接复制运行中的 SQLite 文件。数据库记录的 WAV 路径必须对应容器内的 `/music` 路径；从其他操作系统迁入时，先在独立库副本中使用受支持的重新关联流程适配路径，保留源资料库和 WAV。服务端不会为缺失的 `library.db` 自动创建空库；缺库或开库失败会保持 unhealthy。

运行契约：

- `/data` 保存资料库、快照、缓存和令牌，独立于镜像；`/music` 只读挂载源音频。
- 镜像默认配置位于 `/config/config.toml`，可用只读 bind mount 覆盖。
- 进程使用 UID/GID `10001:10001`，Compose 使用只读根文件系统、`/tmp` tmpfs 和 `no-new-privileges`。
- 端口默认只绑定宿主机 `127.0.0.1:8790`；`AUDIOLIB_SERVER_PORT` 可改端口。公网部署沿用 [部署指南](../server/deploy/README.md) 的 TLS 反向代理和令牌配置。
- 默认配置要求令牌并禁用容器网关的 LAN 信任；不在 Compose、镜像或发布包内保存真实令牌。
- 容器禁用程序内二进制更新。更新时导入新镜像、更改镜像标签并重建容器，保留原数据卷和只读音频挂载。

## 构建与验证

Linux 使用现有 `tools/build-server-release.ps1`、`tools/package-server.ps1` 和 `tools/validate-server-package.ps1`，显式指定 `x86_64-unknown-linux-gnu` 或内部 Docker 输入所需的 `aarch64-unknown-linux-gnu`。打包及运行验收必须在匹配的 Linux 架构环境中执行；ARM 宿主机上的 amd64 仿真应明确记录，不等同于真实 x86_64 硬件覆盖。

Docker 正式流程使用 `tools/package-server-docker.ps1` 和 `tools/validate-server-docker.ps1`，从已通过原生包验收的归档生成独立平台镜像。构建上下文只包含公开 payload、Dockerfile 和无凭据的默认配置，不传入工作目录、Git 元数据或运行数据。完整 payload 包括版本标记、项目许可和 Rust 依赖许可证。源码 Dockerfile 保留为开发示例，不替代正式镜像的提交、许可及包边界验收。

每个平台分别验证版本和提交、镜像架构、非 root 身份、禁用二进制更新、令牌鉴权、隔离合成库和 WAV 流、卷持久化、健康状态及正常退出。测试只使用独占的临时容器和卷，不操作已有服务，不使用真实资料库或源音频。最终报告、截图和日志保持私有。0.8.4 发布六个正式归档及各自 SHA256，共十二个资产；0.8.5 起新增 Windows x64 服务端 ZIP（`AudioLibServer-<tag>-win-x64.zip`）及其 `.sha256`，发布七个归档及各自 SHA256，共十四个资产。

正式包必须绑定最终 main/tag 提交，不能把早期候选产物改名充作正式版。发布后继续按 [AGENTS.md](../AGENTS.md) 同步、校验 Windows 的固定 `local_release/`；服务器镜像发布不包含生产部署。
