# 0.9.5 发布记录

程序 0.9.5 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已于 2026-11-10 14:34:54 UTC（北京时间 22:34:54）正式发布 [v0.9.5](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.5)。七份正式 payload 全部来自最终 main `7d7a60bad31d8640441b16de0d558d3a4211a0e0`；构建、本地验收、十四项线上资产与 SHA256、七份校验正文及发布后固定安装身份同步均通过。内部 Linux arm64 服务端仅作为已验收 Docker 输入。基线 [v0.9.4](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.4) 的标签、资产、校验和历史记录保持原样。

## 范围与兼容性

- 统一平台菜单与全局设置、View / 视图下的主题及语言切换、侧栏入口和有界工具栏提示；保留既有偏好。
- 紧凑搜索任务标签采用关键词优先、目录按剩余宽度显示，完整条件仍可悬停查看。
- Waveform 按原素材采样帧框选、反向框选和端点调整；区间试听、Stop、结束后重播与循环共用实际源身份和播放顺序。
- 选区后台准备并拖出原素材 WAV 切片，保留原采样率、全源声道及对应精度。输出不应用试听变换，不覆盖已有编号文件；只保留目标接受的交付文件，清理只限验证过的自有临时文件。
- 远程显示、试听表示及导出原件通过版本凭据绑定；原件无法证明或已变化时拒绝使用旧表示，原件不可取得时不退回低质量预览。
- Waveform 操作光标、系统 DPI 栅格化和准备反馈；SoundTouch 原生对象配对构造/析构；省略 panic payload、配置与运行参数的同步 Rust panic 记录。
- 构建前按实际产品、目标和 feature 精确预检依赖许可证材料；保留打包与运行验收的独立检查。

源音频、资料库 Metadata 和用户标注保持原有生命周期。切片只重建必要音频结构，不复制描述、封面、cue、循环标记、BEXT、iXML 或未知厂商块，避免错误时间引用及附带原嵌入信息。详见 [选区与切片契约](Rust_Spectral_Audition.md#waveform-选区与-wav-切片)。

API、schema 写目标、快照读取范围与完整格式目录相对 0.9.4 不变。base 格式仍为 `ale-s8-cld1-t3-m2-p1-a1` / `ale-s9-cld1-t3-m2-p1-a1-sd1`；已支持的 af1 格式仍为 `ale-s8-cld1-t3-m2-p1-a1-af1` / `ale-s9-cld1-t3-m2-p1-a1-sd1-af1`，对应 `audio_sources:1`。旧客户端不能因为 schema 数字相同就访问 af1；0.9.4 的升级、备份与降级边界继续有效。

本轮审计了最初登记的 72 个工作树，未修改六个历史 dirty 工作树。旧 [#185](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/185) 的配色、渐变、详情及播放源指示已由 Studio 等后续开发吸收，空页处理已通过 [#240](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/240) 扩展；不能声称该旧 PR 已整体合并。本版没有新增其可配置分页开关与每页 1–1000 条、素材目录显隐状态跨重启保存或 `release-create.sh --draft` 三项独立功能，未更改该 PR 状态。

## 发布与源码身份

| 项目 | 当前身份或状态 |
| --- | --- |
| 已发布基线 | `v0.9.4`，main/tag `f6adb2bd777d4db78d43cbdcee626153592c1482`；既有标签和资产不变 |
| 已审计功能 beta | `a3c0a169625685dc309ed9d39e146272b1279efe`；[#261](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/261)、[#262](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/262)、[#263](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/263)、[#264](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/264)、[#265](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/265)、[#266](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/266) 已合入 |
| 其他分支整合 | 审计完成；旧等价、归档和已取代分支不重放，历史 dirty 工作树保留；#185 的剩余范围如上 |
| 版本准备与 beta 门禁 | [#267](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/267) 合入 beta `134b41629ae1ce8a9d93f6ba8ca301f1b5d92354`；程序和十个内部 package 为 `0.9.5`；版本预检、Mac beta refresh、Windows smoke 通过 |
| main 提升 | [#268](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/268)，最终 [`7d7a60bad31d8640441b16de0d558d3a4211a0e0`](https://github.com/Freedragonlancer/AudioLib-Explorer/commit/7d7a60bad31d8640441b16de0d558d3a4211a0e0)；干净 checkout |
| beta / main tree | 同为 `569b2ed7adb0d5290799a1c760341b56662494d0`；保留 beta 源码验证的原始执行身份，最终交付物另从 main/tag 构建 |
| 正式标签 | annotated `v0.9.5`，tag object `6762954a480f1e0083c78d8f9e959a7b7040fa88`；peeled commit 为上述 main |
| 最终 payload | 七份正式归档与内部 Linux arm64 服务端均已构建、打包并本地验收；每份实际 payload 的对应阶段执行一次 |
| GitHub Release | [AudioLib Explorer v0.9.5](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.5)，Release ID `440455377`；非 draft、非 prerelease，核验时为 latest |
| 发布时间（UTC） | `2026-11-10T14:34:54Z`（北京时间 2026-11-10 22:34:54） |
| 十四项线上资产 | 精确名称及大小写、size、14/14 GitHub SHA256 digest 均匹配已验收字节；实际下载 7 份校验正文/raw bytes/归档 SHA256 全通过，归档下载 fallback=0 |
| 固定 `local_release/` | 本轮安装 verified=907、copied=23；发布后再次逐文件及两 EXE 核对，包与已发布归档身份一致。复用原 CLI `AudioLibCli 0.9.5`、隔离启动及 portable 通过证据，不重复安装套件 |

后续发布记录文档只同步事实，不改变上述二进制源码、标签或已验收归档，不为补写记录重新构建应用。

## 源码验证：原始证据与本轮新增

下表均为复用的历史执行结果，原 base、局部输入清单和二进制身份保留在本机私有 `.rust-validation/`，不作为公开发布资产。2026-11-10 审计确认 `beta-landing-094/accepted-source.json` 与 `waveform-cursor-fine/final-source.json` 各 30/30 输入匹配上述 a3 候选；至 134b 仅版本和文档变化，相关 `app/crates/server/tools/resources/config/vendor/.cargo` 业务实现未变。版本号或说明变化本身不要求重跑未变业务源码，不把下表写成最终 main 上新执行的完整套件。

| 原执行证据 | 原源码与配置身份 | 结果及复用范围 |
| --- | --- | --- |
| `wave-selection-refinement/workspace-result.json`、`initial-check-inputs.json` | `4a1f01b3d8d7795de84d24f87585f2a535c428f0` 加当时记录的局部输入；Rust 1.97.1、Windows x64 MSVC、desktop/CLI ASIO | 全 workspace 严格 Clippy、2396 tests、0 failed；覆盖 SoundTouch 依赖接线后的业务实现。其后改变的 desktop UI 由后述专项覆盖，不声称此历史完整套件针对最终 a3 再执行 |
| `wave-selection-refinement/diagnostic-tests.log`、`diagnostic-clippy.log` | 同一基线加 panic 诊断输入 | 同步 panic 记录的隔离 abort 回归及严格 Clippy 通过；不记录 panic payload |
| `waveform-cursor-scale/checks.json`、`final-source.json` | 同一基线加光标及 UI 局部输入，desktop/all-targets/ASIO | 严格 Clippy、93 Waveform tests 通过；语义缩放 100/150/200/300%，视觉 100/150/200%，系统 DPI 栅格化 100/125/150/200/300% |
| `waveform-cursor-fine/checks.json`、`final-source.json` | 已交付候选 `0.9.4-0+local.c586d85b5cbe`，最终 30 项输入 | 后续仅两个选区 SVG 线宽与说明变化，保留上述源码检查，补充矢量尺寸、透明边界、热点及深浅背景观察；与 a3 内容一致 |
| `menu-native-followup/result.json` | `b506809e0124a9030f15185ad7d4ca8673c48967` 加当时菜单改动 | 所选菜单、焦点、主题、双语和客户端交互回归通过；Windows 菜单已有用户观察确认。保持原证据，后续未改变该菜单实现 |
| `menu-global-settings/review-fixes.log` | 当时已验收菜单/设置候选；隔离合成数据 | 相关配置继承、实例隔离、扫描锁与 Metadata 数据安全回归通过；不沿用被取代包作为本轮正式安装证明 |
| `wave-selection-refinement/playback-ipc.log`、`desktop-close.log` | 当时候选 `0.9.4-0+local.6219178f10c7` | 真实播放/IPC 与关闭排空通过；后续仅光标/UI 改动时复用未变相关行为，最终包另行验收 |
| `wave-selection-refinement/result.json` | 同一任务的实际包和脚本输入 | SoundTouch native-only 包边界及缺失原生材料拒绝回归、portable 通过；不能代替 0.9.5 最终归档验收 |

本轮在 beta `134b416` **新增执行**原生 Mac 检查：`audiolib-audio` / `audiolib-desktop` all-targets、no-default-features、strict Clippy 通过；audio lib tests **180 passed / 0 failed / 3 ignored**，desktop Waveform 专项 **93 passed / 0 failed / 15 ignored**（652 filtered out）。它们是本轮 Mac 结果，不是复用的 Windows 93 项；Windows 专属 SoundTouch 原生堆 fixture 不作为 Mac 覆盖。私有收据为 `release-095/mac-beta/source-checks.private.json`。

最终原生发布管线未传额外 ChecksFile，其 `source_checks=not-requested` 如实保留；上面的复用结果和本轮 Mac 源码检查独立记账。各管线的许可证、最终 build、包边界及实际启动验收均实际执行通过。许可证预检与发布计划的隔离合成回归已有 2026-11-09 结果，见 [管线工具验证](Rust_Release_Pipeline.md#发布缓存和工具验证)；相关规则未再改变，不因新增 tag 重复该回归。

## Beta 两端验收与根范围告警

Mac beta refresh 以 `134b416` 构建、部署成对 server/migrator，health `ok`、served commit 与候选一致，并从每个已配置根读取 4 KiB。未使用 `--with-db`，没有复制生产库或修改生产服务。Windows smoke 使用同一候选服务端和实际已验收客户端：**10 checks passed / 0 failed、20 stream samples passed / 0 failed**，同步 909,342 条记录；快照 10,150.7 MiB，同步 134.2 秒。此为该次实际库和环境的同步条件，不是新增搜索或音频延迟测量。

抽样出现 277 个候选不在当前配置根内，已跳过并解释：该历史库共 909,342 条，当前配置根有 80,562 条、在线且 missing=0，另有 828,780 条属于当前根之外的保留历史记录。根名称、路径及数量与此前已验收 beta 一致，本轮没有扫描或修改配置根、音频、数据库记录。stream 通过范围只覆盖已配置根，**不声明根外历史记录可以播放**。相关私有证据为 `release-095/beta-smoke-result.private.json` 与 `mac-beta/root-scope.private.json`；正常数据环境已恢复，隔离同步临时数据已清理。

## 最终 payload 矩阵

每份正式归档另附 `.sha256`，共七归档、十四资产。Linux arm64 服务端仅为 Docker 内部输入，不作为第八个公开归档。所有平台使用本仓库正式原生计划或 Docker packager/validator；共享 target 串行，独立平台输出隔离。下列目录相对各平台最终 checkout，绝对路径保留在私有收据中，不写入服务地址或凭据。

| Payload / 目标 | 实际配置与环境 | 运行目录或入口 | 本轮本地验收结果 |
| --- | --- | --- | --- |
| Windows x64 客户端 / `x86_64-pc-windows-msvc` | desktop/CLI ASIO，已验证 SDK 2.3.4；Windows | `.rust-validation/release-pipeline/v0.9.5/windows-client/`；`windows-client.json` | license-preflight、build、package、portable、固定安装通过 |
| macOS arm64 客户端 / `aarch64-apple-darwin` | 原生 CoreAudio，ad-hoc 签名；已登录桌面 | `.rust-validation/release-pipeline/v0.9.5/macos-client/`；`macos-client.json` | 精确 client 许可证、构建证明、合并打包/签名/原生运行验收及原生 Quit 通过；未公证 |
| Windows x64 服务端 / `x86_64-pc-windows-msvc` | no-default-features，无播放；Windows | `.rust-validation/release-pipeline/v0.9.5/windows-server/`；`server.json` | 许可证、server/migrator、package、显式 fixture helper、包启动与迁移工具验收通过 |
| macOS arm64 服务端 / `aarch64-apple-darwin` | no-default-features；原生 Mac | `.rust-validation/release-pipeline/v0.9.5/macos-server/`；`server-unix.json` | 最终双二进制、许可证、包结构/隐私、合成库 health 与迁移工具验收通过 |
| Linux x64 服务端 / `x86_64-unknown-linux-gnu` | no-default-features；ARM64 OrbStack 上的 x86_64 仿真 | `.rust-validation/release-pipeline/v0.9.5/linux-x64-server/`；`server-unix.json` | 独立 target/output/run；许可证、最终包、实际启动与迁移工具验收通过 |
| Docker linux/amd64 | 消费已验收 Linux x64 包；同一 ARM64 主机上的 x86_64 仿真 | 独立私有 Docker 运行目录；`package-server-docker.ps1` / `validate-server-docker.ps1` | context/镜像/离线归档检查及实际 stream/auth/持久化/health/SIGTERM 运行验收通过 |
| Docker linux/arm64 | 消费已验收 Linux arm64 内部包；ARM64 OrbStack Linux 环境 | 独立私有 Docker 运行目录；同上正式入口 | 正式打包与运行验收通过，不据容器测试推断物理 Linux 硬件覆盖 |
| 内部 Linux arm64 服务端 / `aarch64-unknown-linux-gnu` | no-default-features；ARM64 OrbStack Linux 环境 | `.rust-validation/release-pipeline/v0.9.5/linux-arm64-server/`；`server-unix.json` | 独立构建、打包与启动验收通过，仅供 arm64 Docker 使用 |

Windows 客户端发布资产名为 `AudioLibExplorer-v0.9.5-win-x64-portable.zip`；本机打包器继续生成 `AudioLibExplorer-latest-win-x64-portable.zip`。其余正式名称为 `AudioLibExplorer-v0.9.5-macos-arm64.zip`、`AudioLibServer-v0.9.5-win-x64.zip`、`AudioLibServer-v0.9.5-macos-arm64.tar.gz`、`AudioLibServer-v0.9.5-linux-x64.tar.gz`、`AudioLibServer-v0.9.5-docker-linux-amd64.tar.gz`、`AudioLibServer-v0.9.5-docker-linux-arm64.tar.gz`。

### 正式发布归档 SHA256

| 正式归档 | SHA256 |
| --- | --- |
| `AudioLibExplorer-v0.9.5-win-x64-portable.zip` | `ebc9e5c76fef8fa15b5762ad9544a8907ba7d0a002b09b96c411b751ec287865` |
| `AudioLibExplorer-v0.9.5-macos-arm64.zip` | `be6fa98481eed522980a6f4339835404d789cadbd9569c88324914fb3d8682bd` |
| `AudioLibServer-v0.9.5-win-x64.zip` | `fa22b7769cf082f93e71f6b1ea0745c468039894edec9ed89e011590b5da18c8` |
| `AudioLibServer-v0.9.5-macos-arm64.tar.gz` | `49d3a67e4199c9f8574c2f6244f9819f3596d8ed89e6bf821856eb2843d195c0` |
| `AudioLibServer-v0.9.5-linux-x64.tar.gz` | `989edf75c052344ae66febaddf47f34603bb03eb56d5b41887cc4790a068496c` |
| `AudioLibServer-v0.9.5-docker-linux-amd64.tar.gz` | `bfac11ca6386a2eb2d9c66d3d075e8af08ec042e8f60af202f4b1c50f1b0466b` |
| `AudioLibServer-v0.9.5-docker-linux-arm64.tar.gz` | `55f9608fe01c5291ab1a170bfe8ba8faa1ea8534de448f9c993f63fd1ab73cfa` |

## 线上核验、固定安装与缓存维护

独立发布核验确认 `pubverified=true`：Release 标题、tag、公开状态和精确十四项资产通过；GitHub digest、size 与本地验收字节匹配，实际下载七份校验文件并验证正文及归档 SHA256，未触发归档下载 fallback。标签对象 `6762954` → `7d7a60b` 及全部 asset ID 在核验前后不变。公开 Release 正文 SHA256 为 `47d841859819791ba31e937e216097cd51304a1680119b4e50c6ed939bdd5436`，与获准发布的正文一致。私有证据为 `release-095/publication-preflight/published-verification.latest.private.json`。

发布后依据原安装清单重新核对 `local_release/` 全部 907 个包文件、两个 EXE、portable payload 与线上 Windows 客户端归档；归档 SHA256 为上表 `ebc9e5c7…ec287865`。已安装文件未变，因此复用原 CLI 0.9.5、隔离桌面启动及 portable 套件，不重复构建、安装或整套运行验收。正常数据目录、资料库、Metadata、标注和偏好保留。私有证据为 `release-095/post-published-install.private.json`。

Windows 本轮明确登记的 MSVC 子 target 已执行 40 GiB Debug 预算维护：通过且预算满足，removed=0，剩余逻辑估算 **15.897 GiB**。Mac 仅对本轮具有真实 Cargo marker 的 `aarch64-apple-darwin` 子 target，在核对 parent 所有权、四个 release 二进制 SHA256 和无活动编译后正式登记，执行一次相同的 Debug 40 GiB 预算维护；2026-11-10 14:41:43 UTC 通过，removed=0 B，剩余 **3,979,320,814 B（3.706 GiB）**，预算满足，维护后四个二进制摘要不变。私有证据为 `release-095/cache-windows-child.private.json` 和 `mac-final/cache-maintenance.private.json`。

旧 parent/host profiles、旧系统盘、未使用的历史 NAS 和未纳入本次维护的 Linux 缓存保留，未扫描或删除；不把这些未检查的历史缓存描述成已清理。release 输出、SDK、正式包和安装保留。

文档收尾只同步事实，不移动 tag、不重建应用，不把已验收字节改成后续文档提交身份。

## 覆盖限制

Explorer/Finder/真实 DAW 接收、释放鼠标后的文件可读性及长期引用仍未做跨程序实际验收；本机原生启动、内部拖出状态回归及合成切片通过不能替代它。Linux 桌面原生拖出当前明确不支持。Mac 客户端为 ad-hoc 签名，未经 Apple 公证。

Windows server 包启动验收不证明作为真实服务的优雅关闭；Linux x64/Docker amd64 为 ARM64 主机上的仿真，未声明物理 x64 Linux 硬件覆盖。

此前有单次隔离 WASAPI `RPC_E_CHANGED_MODE`，干净重启后两段选区均正常结束，尚未建立原因；原先默认参数间歇直接退出的问题后续未再复现，不能据 SoundTouch 析构修正宣称已找到该退出根因。相关历史失败与观察保留在私有证据中。

本轮尚无真实 ASIO 硬件、听感盲测或按钮到模拟输出的起音测量。P95 latency <500ms for 1M+ records 仍是项目目标，本版未改搜索性能路径，不据此重复百万记录基准或宣称新的性能实测。
