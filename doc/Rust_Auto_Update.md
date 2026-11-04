# 自动更新

支持自动更新的 Windows 客户端与服务端在每次正常启动后，从 GitHub 公共仓库
[`Freedragonlancer/AudioLib-Explorer`](https://github.com/Freedragonlancer/AudioLib-Explorer/releases)
查询已发布的 Release。查询在后台进行；断网、超时或 GitHub 限流会显示检查失败，
不会阻止打开应用或启动服务。诊断和自动化冒烟模式不做网络更新检查；
`AUDIOLIB_UPDATE_DISABLE=1` 可在受管理的安装环境中关闭更新。

## 使用

Windows 客户端发现新版本后显示更新提示。确认下载与更新后，程序校验下载的包，
通过现有关闭流程保存 Metadata、排空请求的写入与扫描，再部署并重启。
暂不更新可关闭提示；手动检查入口可以重新查询。

服务端在现有管理页面显示检查状态与可用版本。管理员确认具体 Release 后，
服务器在后台下载校验，并通过正常停止流程排空工作后部署。更新接口沿用管理页面的
访问控制，不接收任意下载地址，也不触及用户库文件。
资料库正在打开或打开失败时也可下载修复版本；部署前仍须等待启动线程及其缓存写入
安全退出，关闭失败或超时会取消部署并保留原程序。已有扫描须先完成或取消。

自动更新客户端安装文件为 Windows x64 portable 包；服务端按运行平台选择 Windows x64 ZIP、
Linux x64 tar.gz 或 macOS arm64 tar.gz。没有对应平台产物或校验和的 Release 不作为
可安装更新。服务安装的重启与监督方式见 [server deployment](../server/deploy/README.md)。

0.8.3 同时准备 macOS arm64 桌面客户端 `AudioLibExplorer-<tag>-macos-arm64.zip`，
内含 `AudioLib Explorer.app`。初期采用手动安装与更新，不能把这个 bundle 送入
现有 Windows 平铺更新器，也不能放宽旧客户端的公共文件清单来适配它。
替换 Mac app 前先正常退出，保留原有资料库与偏好数据。详见
[Mac 客户端指南](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/MACOS_README.md)。
首次分发计划为 ad-hoc 签名、未经 Apple 公证；原生签名与启动验收仍须在 Mac 上实际完成。
新增此 Release 资产不改变 Windows 客户端或 Mac 服务端的自动更新协议。

0.8.4 起正式发布 Linux x64 原生包，按既有服务端平台规则提供更新。Docker amd64/arm64 离线镜像设置 `AUDIOLIB_UPDATE_DISABLE=1`，通过导入新镜像并重建容器更新；独立 Linux arm64 原生包不作为 Release 资产发布。镜像交付不改变原生更新包的公共文件策略。详见 [Linux 与 Docker 交付](Rust_Linux_Docker_Release.md)。

## 渠道与版本

安装目录保存两个很小的程序元数据文件，与用户的 `settings.json` 和资料库分开：

| 文件 | 内容 | 作用 |
| --- | --- | --- |
| `update-channel.txt` | `stable` 或 `beta` | 决定是否接收预发布版本 |
| `update-version.txt` | 完整 SemVer，例如 `0.6.2-beta.12` | 区分共用 `0.6.2` Cargo 程序版本的不同 beta 包 |

stable 只接收正式 Release；beta 接收 beta、RC 和正式 Release。
按 SemVer 比较，仅提示比当前安装版本新的版本，忽略草稿和不支持的标签。
安装到 beta 后，即使更新到正式 Release，也保留 beta 渠道。
没有元数据的旧安装根据程序版本判断，普通 `X.Y.Z` 程序默认 stable；
有后缀的程序默认 beta。元数据基础版本与运行 EXE 不符时停止更新，并提示修复安装。

本地未指定标签的包使用 workspace 版本，不伪造 beta 发布号。
Windows 的 `install-beta.ps1 -Development` 将本地测试安装标记为
`X.Y.Z-0+local.<GUI EXE SHA256 前 12 位>`，并写入仅供安装使用的
`update-development.txt`。完整标识显示在“关于”和更新窗口中。
该标记不进入公开 portable 包，也不属于更新文件白名单。

本地 Windows 开发安装关闭启动检查及手动检查、安装入口，通过再次执行安装脚本
更新测试构建。桌面程序识别精确的 `-0` 开发标记，兼容旧 `X.Y.Z-0`；
Beta 启动器还会根据开发安装文件设置 `AUDIOLIB_UPDATE_DISABLE=1`，保护此前的程序。
构建元数据仅区分二进制身份，不改变 SemVer 排序：不能依赖版本号本身阻止
`X.Y.Z-0` 被较早发布但排序更高的 `X.Y.Z` 覆盖。

安装已发布 Beta 时不传 `-Development`：保留包的完整版本，移除本工具拥有的开发
标记，恢复原有更新通道；已发布的正式版不会降级到同版本 beta。
Mac 上的 `beta-refresh.sh` 仍保留 beta HEAD 的精确发布标签，无标签时写入
`X.Y.Z-0` 开发基线；服务端现有的管理员确认更新流程不受 Windows 客户端策略影响。

## 下载与部署边界

更新仅使用上述固定 GitHub 仓库的命名 Release 产物与同名 `.sha256` 文件，
在完整下载后核对 SHA-256，再解包到应用拥有的 staging 目录。
下载、解包大小和等待时间有限制；路径穿越、链接和不在发布清单内的文件会被拒绝。
SHA-256 校验依赖 GitHub HTTPS 与发布者账号的可信性，不是独立的发布签名。

替换范围限于发布包拥有的程序、公共文档、资源、示例配置和许可证。
数据库、SQLite sidecar、`settings.json`、日志、缓存和用户音频从不属于更新范围。
更新会保留替换前的程序文件，用于部署失败时回滚。无法写入安装目录或程序文件被占用时，
更新失败并保留原安装，不强制终止其他实例。Windows 使用独立 helper 等待原进程退出；
正常退出屏障未完成时，不应替换运行中的安装。

staging 位于安装目录的 `.audiolib-update-*` 中。已校验的压缩包在解包后删除；
已部署版本的回滚副本与 Windows helper 会保留，结果写入安装目录的
`.audiolib-update-result.txt`。本版不会自动删除这些恢复目录，清理时须确认更新已结束、
helper 已退出，并保留仍需恢复的备份。

更新器使用当前程序内置的公共文件清单。打包同时检查
`tools/update-compatible-files.json` 中冻结的 v0.7.8 更新器清单；新增公共文件即使
通过当前发布清单的隐私检查，只要旧更新器不认识，也会在替换打包输出前失败。
不可只更新该兼容基线来消除报错；改变包布局须先设计旧客户端可安装的过渡版本，
或明确宣布需要手动升级的最低版本边界。

原 v0.8.1 包新增的 `doc/Rust_Artwork.md`、`doc/Rust_Shared_Albums.md` 和
`doc/Rust_Search_Tasks.md` 会使 v0.7.8 报 `Non-public update file`，下载完成后仍无法
安装。兼容打包已移除这三份可选文档，保留仓库文档及随包文档中的在线链接。
这属于包布局问题，不表示自动更新功能被移除；仅修改新版本代码无法改变已安装
旧客户端的清单。兼容包将通过新标签 v0.8.2 发布，原 v0.8.1 标签和资产保持原样；
新版本发布后，v0.7.8 可检查并升级到新的兼容包。冻结文件名校验仅证明包布局兼容，
不代表已经完成旧版二进制的完整下载、解包、安装和重启验收。
在新兼容版本发布前，可正常关闭旧客户端后，把完整 Windows portable 包覆盖到原程序
目录（包括两个 EXE 与随包文件）；保留现有数据目录、配置、资料库和 SQLite sidecar，
不要用测试库或空库替换工作库。服务器部署仍按其现有部署和回滚流程处理。

当前打包器的平铺公共包和历史 Windows 客户端不带 `v` 的
平铺包命名均受支持。已发布的 v0.5.0、v0.5.1、v0.6.0、v0.6.1、v0.6.2 中小写
`audiolib-server-<版本>-macos-arm64.tar.gz` 是套一层版本目录的单二进制包，缺少
公共许可证文件，部分还含 AppleDouble 文件；这些已知布局不会被提示为可安装更新。
部署这些旧 macOS 包须走手动流程，不能绕过更新器的公共文件及许可证边界。

## 生成可更新的发布包

发布标签必须与 workspace 基础版本一致：

```powershell
./tools/build-release.ps1
./tools/package-portable.ps1 -SkipBuild -Tag v0.6.2-beta.1
./tools/validate-portable.ps1 -BinaryDirectory dist/latest/portable -CleanCheckout
./tools/install-beta.ps1
./tools/package-server.ps1 -Tag v0.6.2-beta.1 -Target x86_64-pc-windows-msvc -Asio
```

示例标签应替换为本次真实候选标签。`ci-release.ps1 -Tag` 会把同一标签传给 portable
打包器；server 打包器始终要求 `-Tag`。省略 portable 的 `-Tag` 会写入 workspace
正式版号与 stable 渠道，适用于本地开发包，不代表该版本已经发布。
`install-beta.ps1` 保留包中的完整版本号，单独把安装渠道设为 beta，并保留 beta 启动器。
未打标签的本地 beta 开发安装使用 `./tools/install-beta.ps1 -Development`。

上传前仍须完成仓库要求的构建、包边界、隔离启动和相关功能验收。
更新功能不会创建标签或发布 GitHub Release，也不会把本地数据库上传。
固定 `local_release/` 与隔离 `beta_release/` 的安装、数据目录约定见 [AGENTS.md](../AGENTS.md)。
