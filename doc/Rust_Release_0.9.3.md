# 0.9.3 发布记录

程序 0.9.3 | API `/api/v1/` | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已于北京时间 2026-11-07 14:19:35 首次正式发布。原始七份 payload、十四项线上资产及当时的固定安装同步完成验收；下文保留该次发布身份与证据。用户随后明确授权替换同一 Release 的两个客户端归档及各自校验文件，当前客户端与保留的服务端资产来源分别列出。

## 原始发布身份与状态（历史）

| 项目 | 结果 |
| --- | --- |
| 版本与正式标签 | `0.9.3` / annotated tag `v0.9.3` |
| 最终 main / tag 提交 | `5ca7e59c7fcadc22043f0a030c8d496854809b23` |
| 已验收 beta 提交 | `d88a0c66085febcca6249080be71613534d2e276`，与最终 main 源码树相同 |
| 最终平台交付 | 七份 payload 均从上述最终 main/tag 身份构建、打包并实际验收通过 |
| GitHub Release | [AudioLib Explorer v0.9.3](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.3)；已发布，且已核实为 latest 正式版 |
| 正式发布时间 | 2026-11-07 06:19:35 UTC（北京时间 14:19:35） |
| 远端完整性复核 | 标题、annotated tag 对象链和提交、非 draft / 非 prerelease、latest、精确十四项资产均通过；十四个 GitHub 服务端 SHA256 摘要与验收字节一致，七份公开校验文件下载复核通过 |
| 固定 Windows 安装 | `local_release/` 已更新为 `0.9.3`；全部 package-owned 文件 SHA256、CLI 版本与一次隔离桌面启动通过 |
| 发布后安装同步 | 正式 Windows ZIP 的 GitHub 摘要与本地验收归档一致；发布后复核 907 个包文件 SHA256、CLI 0.9.3 和一次隔离 GUI 启动通过，文件均已相同，无须再次复制 |
| 收尾缓存维护 | 本轮已验证 Windows 构建缓存含宿主保留 profiles：逻辑估算 45,011,256,261 → 15,601,754,094 字节（14.530 GiB），移除增量缓存 29,409,502,167 字节，40 GiB 预算满足；release 输出保留，未验证历史缓存保留 |

上述为原始发布记录。此次明确授权的客户端替换使用下文独立记录的新源码身份；`v0.9.3` 的 tag 对象及其提交保持原样，其他五份归档及各自校验文件保留原始发布字节。

## 本版变更

- [#239](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/239)：修复 Waveform 已播放区域接缝，并明确 Play 按钮的 disabled / ready / playing / paused 状态反馈。
- [#240](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/240)：服务器移除遮罩使原始页整页不可见时，继续有界双向浏览；保留任务恢复、窗口裁剪、空 Previous 和取消后继续读取能力。没有整合旧 #185 的视觉重做、分页偏好或草稿工具等新功能。
- [#241](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/241)：部署工具将新建的自有 SQLite WAL 备份副本完成为可独立读取的镜像，再执行只读校验；原始库仍按只读源处理。
- [#243](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/243)：补齐 Mac 实际依赖图缺少的精确版本许可 notice，并增加版本、SPDX、VCS、正文和出处篡改回归。未改动依赖版本或 features。

API、数据库格式、本地及服务端写目标和快照读取范围保持原契约；本版没有新增数据库迁移。#241 与 #243 均未改变应用 Rust 源码。

## 原始发布验证范围与证据复用

原始发布的桌面验证使用 Rust `1.97.1-x86_64-pc-windows-msvc` 和发布所选 `asio`，已通过一次格式检查、桌面 all-targets strict Clippy，以及 `search_tasks::tests` 58 个父入口/直接测试、`result_window::tests` 7 个、`result_page_navigation::tests` 8 个；54 个隔离 child 入口由相应父测试调用。覆盖整页隐藏、多原始跨度双向加载、Path cursor / limit、600 行窗口上限、任务恢复和 revision 变化后的重建。单次查询最多 200 行，持久化工作区格式未改变。

后续部署、许可和文档变更未改变上述相关应用/测试输入、features 或工具链，因此复用仍有效的源码检查，不描述为在每个平台重新执行。部署备份回归包含 Windows 完整 69 案例及 Mac 真实系统 SQLite 10 案例；Mac 实际选中依赖的 492 项许可计划与相关许可回归通过。最终各平台包的验收另以最终 main/tag 实物为准。

#239 的原始视觉结果按原身份复用：

| 原始证据身份 | 值 |
| --- | --- |
| 被验证功能提交 | `ced087b9fda57d93afd0a8410662b1f67c84fdea` |
| 合入 beta 的提交 | `abbc6024c648fa6b8cacac22acba1c37ef636f1f` |
| 当时二进制 | `0.9.2 local GUI fixes on beta base 306e07d`，不是本次正式发布包 |
| 原始 GUI SHA256 | `d7618b84a4d256f2eb8bd3b53b709c287e49a2ed8de661cb9c21bfe4da80ea88` |
| 原始 CLI SHA256 | `1895cfaf841d5df11e4fab025672f01eaaabc1f9208f536204d80a331cf60594` |

原始记录包括 2 个 Waveform 回归、4,800 次像素比较、100% / 125% / 150% / 175% / 200% DPI 共 24,160 帧、12,800 个大坐标浮点案例，以及四种 Play 状态。没有将这些旧二进制改名作为最终 0.9.3 payload。

beta 提升证据同样保留其实际身份：

| gate | 实际身份与结果 |
| --- | --- |
| Mac beta 服务端 | `03c45508d0f15c2515eebe5727205df4c172d9bc`；刷新后 health、提交身份及配置根读取通过 |
| Windows beta smoke | 客户端来自 `e1534779d3555fb754c571762b4e28852e83e0f2`；与后续 beta 的相关应用输入相同；连接上述 `03c455…` 服务，完整同步 909,342 条并完成 20 次流读取 |
| Mac 标签前候选 | `d88a0c66085febcca6249080be71613534d2e276`；实际构建、打包和原生验收通过 |
| 原始正式包 | 七份均绑定 `5ca7e59c7fcadc22043f0a030c8d496854809b23` / `v0.9.3`，另行完成其正式包验收 |

许可目录属于流程与打包范围，按 [AGENTS.md](../AGENTS.md) 复用未变服务端和 Windows 功能证据；没有把运行中的 beta 服务描述为已部署 `d88a0c…` 或最终 main。所有测试库和包验收 fixture 均为自有隔离数据。

## 正式资产

以下为客户端替换后的七个归档及完整 SHA256；每个归档附同名 `.sha256`，仍为精确十四项资产。两个客户端来自替换源码 `81ac352fdd73a009197a1442887dc65256aca302`；其他五份归档保留原 tag 源码与原始 SHA256。替换后的远端核验结果见下文，原始验收记录按其真实身份保留。报告、截图、配置、日志、fixture 和数据库不作为资产发布。

| 产物 | 归档 | SHA256 | 公开校验文件 |
| --- | --- | --- | --- |
| Windows x64 客户端，默认 ASIO | [AudioLibExplorer-v0.9.3-win-x64-portable.zip](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibExplorer-v0.9.3-win-x64-portable.zip) | `cd725469ba100bad5953cff1ffa9e21d920c72dbcc2f501cf3f2f1007f5ccc5a` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibExplorer-v0.9.3-win-x64-portable.zip.sha256) |
| macOS arm64 客户端 | [AudioLibExplorer-v0.9.3-macos-arm64.zip](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibExplorer-v0.9.3-macos-arm64.zip) | `30a7d24e24c5ee8ef4c3e373f9ab2643d573472bc59bba156d695f6ff0300f74` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibExplorer-v0.9.3-macos-arm64.zip.sha256) |
| Windows x64 服务端 | [AudioLibServer-v0.9.3-win-x64.zip](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-win-x64.zip) | `cec23f0355ac16ee1d8d11b8183ca260366c603750b87d5fa12f022dce3409fc` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-win-x64.zip.sha256) |
| macOS arm64 服务端 | [AudioLibServer-v0.9.3-macos-arm64.tar.gz](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-macos-arm64.tar.gz) | `c720273188c933e29954428509e5cb22f9b7e7ce1a12c6dcca46914d731518e8` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-macos-arm64.tar.gz.sha256) |
| GNU/Linux x64 服务端 | [AudioLibServer-v0.9.3-linux-x64.tar.gz](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-linux-x64.tar.gz) | `6e0a5959537272cea143d61085877f0e24e56979b833234769fab54e5076b559` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-linux-x64.tar.gz.sha256) |
| Docker linux/amd64 离线镜像 | [AudioLibServer-v0.9.3-docker-linux-amd64.tar.gz](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-docker-linux-amd64.tar.gz) | `47221c0947830fa42e754e83d70c6e37233e09539e736cafcc582eb508c7aef1` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-docker-linux-amd64.tar.gz.sha256) |
| Docker linux/arm64 离线镜像 | [AudioLibServer-v0.9.3-docker-linux-arm64.tar.gz](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-docker-linux-arm64.tar.gz) | `36c9ac10b2abf1c8972bc943443ac4f0fffcd73e34cf01a8779c9663a3bda523` | [.sha256](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/download/v0.9.3/AudioLibServer-v0.9.3-docker-linux-arm64.tar.gz.sha256) |

Linux arm64 服务端原生包只作为 Docker 内部输入，不单独发布。Windows 客户端的本地 `latest` 归档仅在组装时映射为上表正式名称，归档字节未改变。

## 客户端替换（2026-11-07 20:44:57，北京时间）

本次按用户明确要求，在已发布的 `v0.9.3` 上替换 Windows x64 / macOS arm64 两个客户端归档及各自 `.sha256`，共四项资产。此项授权仅用于补齐播放控件的统一反馈，不改变通常从最终 tag 构建新版本的发布流程。

| 项目 | 此次替换身份与结果 |
| --- | --- |
| 保留的原始 tag 提交 | `v0.9.3` → `5ca7e59c7fcadc22043f0a030c8d496854809b23`；tag 对象及提交未移动 |
| 两个客户端的新源码 | main `81ac352fdd73a009197a1442887dc65256aca302`（[#247](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/247)，包含 [#246](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/246) 的 `fed59ef` 修复） |
| 界面修订 | Classic 播放、暂停、停止、循环和 Mono 统一移除激活下划线，使用已有边框与底色反馈；保留 checkable / checked、键盘、启用条件与播放命令 |
| Windows 新 payload 验收 | MSVC 原生 release、经验证的 ASIO SDK 2.3.4、许可与公开包边界、隔离合成库 CLI/GUI 启动验收通过 |
| Mac 新 payload 验收 | arm64 原生 release、902 个 bundle 文件、CLI、隔离 GUI 启动及正常退出验收通过；ad-hoc 签名，未经 Apple 公证 |
| 远端替换后核验 | 精确十四项资产核对通过；新四项 GitHub digest 与公开回下载文件均匹配已验收字节，其余十项资产的 ID、名称、大小和 digest 保持原样，原 annotated tag 对象及提交未移动 |
| 发布后固定安装同步 | 已同步固定 `local_release/`，907 个包拥有文件的 SHA256 与已发布 Windows payload 一致，AudioLibCli 0.9.3 与隔离桌面启动通过；原资料库与偏好设置保留 |
| 此次缓存维护 | 对已登记 Windows target 子目录完成一次维护；计入保留的宿主 profiles 后合计 15601635736 字节（约 14.530 GiB），满足本轮 40 GiB 预算，未移除文件。父级缺 Cargo 标记的宿主 profiles、历史 NAS 及未登记 Mac/Linux/Docker 缓存保持原样，不声明全局预算达标 |

保留的五个归档为 Windows / macOS / Linux 原生服务端，以及 Docker linux/amd64 / linux/arm64；它们仍来自原 tag 提交，未重新构建、重打包或上传。此表与上方当前资产表共同描述混合来源，不将此次两个客户端描述为原 tag 构建，也不将五个原始服务端产物描述为新源码构建。

已经安装 `0.9.3` 的用户需要手动重新下载对应客户端并按原安装方式更新；版本号未变，现有更新器不会为相同版本再次提示升级。Mac 客户端继续使用手动安装，ad-hoc 签名不代表 Apple 公证。此次只调整界面反馈，不改变 API、数据库格式或源音频。

包内文档保留各自构建提交中的历史快照；此次替换后的公开记录由本次外部 Release 说明及后续文档提交补充。后续文档提交不作为已构建二进制的源码身份，也不触发重打包。

原始客户端归档 SHA256 保留作历史记录，已不对应当前同名下载资产：

| 原始客户端归档 | 替换前 SHA256 |
| --- | --- |
| `AudioLibExplorer-v0.9.3-win-x64-portable.zip` | `47b6cb7369aad727a7aff3f125f7d34034f71b4b8b7c3bdeb288d9c55c046aaa` |
| `AudioLibExplorer-v0.9.3-macos-arm64.zip` | `926f2cc863ff126ff0dd10b64185352da49f0cfb806a03eac4e38eb203c9c649` |

## 验收边界与使用限制

- Mac 客户端在已登录桌面的 arm64 Mac 完成构建、签名、包结构、CLI、隔离 GUI 启动和正常退出验收；签名为 ad-hoc，未经 Apple 公证，使用手动安装和更新。
- Mac、Windows、Linux 服务端及两个 Docker 平台均完成各自实际包的结构、许可、二进制身份及隔离 health / fixture 验收。Docker 消费已验收的对应 Linux 包，更新方式是替换镜像。
- Linux amd64 与 Docker amd64 在 ARM64 主机上仿真运行，未覆盖物理 x64 Linux 硬件；arm64 使用匹配架构的 Linux 容器。
- beta 的根范围统计中，80,562 条位于当前配置根内，828,780 条位于其外，是原库已有的配置路径分布。该统计不是逐文件可读性审计；909,342 条同步及 20 次流读取也不代表读取过全部源音频。
- 没有新增声学输出延迟、真实 ASIO 硬件或百万记录性能测量；不将构建、窗口启动或已有视觉矩阵记为新硬件性能指标。
- Mac 服务端归档保留平台生成的 provenance 元数据；跨主机原始 tar 元数据条目与 Mac 原生逻辑 payload 计数不同。私有只读检查确认仅为相同的 `com.apple.provenance` 及时间戳，未发现私有路径、地址或凭据；原始已验收归档及 SHA256 保持不变。

Windows 固定安装与运行数据分离：程序使用 `local_release/Start-AudioLibExplorer.cmd`，正常资料库、Metadata、设置和扫描历史保留在原数据目录。隔离启动验收未覆盖或替换工作库，结束后恢复正常环境。

原始发布的缓存维护在当时全部构建、验收及发布后安装同步完成后执行。先对带有真实 Cargo 标记、归属已验证的该轮目标子目录显式登记；将一次只读测得的宿主 profiles 5,910,494,891 字节计入本轮 Windows 总量后，仅清理增量缓存，使合计保留量满足 40 GiB。宿主 profiles 因父目录缺少根标记保持原样；历史 NAS 权限受限缓存以及未登记的 Mac、Linux 和 Docker 外部缓存均保留，不声称全局缓存预算达标。上述移除量为逻辑估算，未测量磁盘物理释放量；SDK、Cargo/Rustup home、dist、安装、验证证据和音频资料库未作为清理对象。
