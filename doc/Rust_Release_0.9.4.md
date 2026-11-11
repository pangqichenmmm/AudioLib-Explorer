# 0.9.4 发布记录

程序 0.9.4 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已于 2026-11-08 15:28:34 UTC（北京时间 23:28:34）正式发布 [v0.9.4](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.4)。本版以 `v0.9.3` 为基线，交付 MP3、AIFF/BWF 音频源、RIFF 边界与 MP3 gapless 修复，以及自适应搜索任务标签。七份正式 payload 全部来自最终 main `f6adb2bd777d4db78d43cbdcee626153592c1482`；十四项资产的远端身份、GitHub digest、校验正文及发布后固定安装同步均已通过。

## 发布与源码身份

| 项目 | 身份与状态 |
| --- | --- |
| 版本基线 | [v0.9.3](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.3)，原 tag `5ca7e59c7fcadc22043f0a030c8d496854809b23` 保持不变 |
| 0.9.3 两份客户端历史 | 来自 main `81ac352fdd73a009197a1442887dc65256aca302` 的替换；其余五份归档保留原 tag 身份，详见 [0.9.3 发布记录](Rust_Release_0.9.3.md) |
| beta 功能基线 | `e8c0ba1f0566651da7bf647831811a933414a994`；[#255](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/255)、[#256](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/256) |
| 准备与提升 | [#257](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/257) 合入 beta `b28822910d731bc1cda27c4680cfb153bbebb8fb`；[#258](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/258) 提升 main |
| 七份产物的唯一源码 | [`f6adb2bd777d4db78d43cbdcee626153592c1482`](https://github.com/Freedragonlancer/AudioLib-Explorer/commit/f6adb2bd777d4db78d43cbdcee626153592c1482)，干净 checkout；程序及十个内部 package 均为 `0.9.4` |
| 标签 | annotated `v0.9.4`，tag object `0aedfc6c323e1dfc119d06d7b3a6513d259126d7`，远端 peeled commit 为上述 f6；核验前后不变 |
| GitHub Release | [AudioLib Explorer v0.9.4](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.4)，Release ID `438323921`；非 draft、非 prerelease，核验时为 latest |
| 发布时间（UTC） | `2026-11-08T15:28:34Z`（北京时间 2026-11-08 23:28:34） |
| 线上资产与 digest | 精确 14 项，14/14 GitHub SHA256 digest 与 size 匹配已验收字节；下载 7 份校验正文通过，归档下载 fallback 为 0 |
| 发布后固定安装同步 | 原固定 `local_release/`；copied=0、verified=908，CLI `AudioLibCli 0.9.4`、隔离桌面启动及正常环境恢复均通过 |

后续发布记录文档提交仅同步事实，不是新的二进制源码身份；不移动 `v0.9.4`、不重建已验收归档。0.9.3 的 Classic 播放控件反馈属于此前两份客户端替换，不重复列为本版新增功能。

## 本版变更

- 默认扫描新增 `.mp3`、`.aif`、`.aiff`、`.bwf`，以实际容器字节识别格式，包含远程 opaque `.wav` 身份；已有 WAV、FLAC、Ogg/OGA 支持保留。
- RIFF 子块遍历限制在父容器声明范围内，RF64/BW64 使用合法前置 ds64 的 64 位长度。容器外尾信息只告警，不污染 Metadata；真实内部截断、错误布局和不支持的长度规则仍明确拒绝。
- MP3 保留有界 ID3v2、COMM/TXXX、未知重复文本和 ID3v1 fallback，封面仅在请求时提取，二进制不进入 Metadata JSON。分别验证实际帧数、信息帧存在性、gapless trim 与缓存布局，坏可选计数不能截短音频。
- AIFF 保留 NAME、AUTH、ANNO、COMT 上下文及 ID3；支持 signed PCM 1–32 valid bits、AIFC NONE/twos/sowt 与 big-endian fl32/fl64。其他 AIFC 压缩及 block-aligned SSND 不支持，`.aifc` 不在新增默认扫描扩展名中。
- 非 WAV 远程试听继续使用原始 `/stream` 字节。MP3 可信合成信息帧仅存在于解码器内存；源音频、HTTP 响应与完整下载缓存不被改写。AIFF 试听、seek 和 Waveform 共享真实存储布局。
- 搜索标签先共享可用宽度，最小 144 逻辑像素后再显示溢出导航，并为滚动条保留独立空间；关键词优先，目录和前缀按空间让位。
- 缓存工具正确读取 Hidden 属性的所有权标记，补充真实 Hidden 标记、边界和管线交付回归；不扩大外部缓存认领范围。

## AF1 升级与兼容

本地写目标仍为 schema 8，服务端显式迁移目标仍为 schema 9，客户端快照读取范围仍为 3..=9，HTTP 为 `/api/v1/`。相对 v0.9.3 的 PATCH 选择符合[版本规则](Rust_Versioning.md#规则)，不代表所有完整数据库格式都与旧客户端兼容。

| 库中实际能力 | schema 8 精确格式 | schema 9 精确格式 |
| --- | --- | --- |
| 尚未启用扩展源 | `ale-s8-cld1-t3-m2-p1-a1` | `ale-s9-cld1-t3-m2-p1-a1-sd1` |
| 必需 feature `audio_sources:1` | `ale-s8-cld1-t3-m2-p1-a1-af1` | `ale-s9-cld1-t3-m2-p1-a1-sd1-af1` |

AF1 在首次成功写入扩展源记录的同一事务中启用；原格式库和空库保持原 ID，删除新格式记录不会自动撤销能力要求。导入、迁移、回滚和派生状态重建保留实际 AF1 契约。

旧客户端不能仅凭 schema 数字相同打开 AF1。既有 schema-9 WAV-only 兼容投影继续排除新增容器（包括伪装成 `.wav` 的非 WAV），恢复历史 schema-8 投影身份；它不向旧客户端提供新源能力，也不是任意 schema-8 AF1 库的通用降级方式。

升级前先备份资料库，关闭共享本地库的旧版读写实例，再更新客户端、CLI 和服务端，最后扫描新格式。新检查不能追溯约束已打开的旧 SQLite 实例。回退旧程序需要匹配的兼容备份或受支持的只读投影，不能手工删除 feature 标记。安装更新保留正常数据目录、资料库、标注和偏好。

## 功能检查的原始身份

以下为复用的原始功能证据，不冒充最终 f6 正式包的新执行结果。相关源码、features、工具链与环境保持一致时复用通过证据；版本号、标签和文档变化不使无关测试失效。

| 范围 | 原始身份与实际覆盖 |
| --- | --- |
| Windows 源码与应用 | 基于 `ff45063` 的 dirty 工作区，程序显示 0.9.3；发布所选 ASIO 客户端及无默认特性服务端。格式、strict Clippy、原有通过目标和最终受影响目标补跑组成通过证据，非单次全 workspace 从未失败 |
| 音频核心 | 157 passed、0 failed、3 ignored；包含 RIFF/RF64/BW64、AIFF PCM/float、MP3 原生 Symphonia 逐样本参考、gapless、非零 anchor seek、缓存声明与坏信息帧 |
| 数据安全与消费者 | 受影响库管理、review fixes、ACL、播放 IPC、迁移、服务端格式、快照及旧 golden/统计 fixture 修正后的目标通过，保留各自输入与日志 |
| beta 源码一致性 | 原工作区 51 tracked 与 16 untracked，共 67 项源码/fixture，逐项与 e8c0ba1 一致；两个功能 PR 已合入，Linux (fmt) 只证明格式 |
| Mac beta 服务端 | 功能验收来自 `36b7ca8df30ebe42402afe044e17ef464d09c8c6`；至 e8 仅四个客户端 UI/测试/文档文件变化，因此相关服务端证据复用。最终 e8 beta 原生包另有独立收据 |
| beta 远程端到端 | MP3/AIFF/BWF 各一份自生成 30 秒 48 kHz mono 音频；扫描、Metadata/FTS、MIME、Range、完整原始 SHA256、标注保留和 GUI playing/seek/ended 通过；最终 e8 冒烟 9 checks、20/20 Range 通过 |
| 缓存工具合成回归 | Windows 与 macOS arm64 的正式 test-build-artifacts 合成回归 exit 0，含 Hidden owner、边界及管线成功/失败交付。Mac PowerShell 7.6.6；非旧 beta 真实缓存维护补跑 |

临时端到端 root 和三条记录已移除，源 SHA256 保留，已启用的 AF1 要求持续存在。原始私有日志、地址、配置、数据库与证明不作为 Release 资产。

## 最终 f6 payload 验收

`check-release -Tag v0.9.4 -BaseTag v0.9.3` 通过，`schemaChanged=false`。两条最终 Windows 管线 `source_checks=not-requested`，对应源码检查按上述身份复用；实际构建、打包、验收和客户端安装已执行。Linux amd64 另执行一次 release 新格式源码回归并通过，内部 Linux arm64 对应源码回归为 `not-requested`。

| payload | 实际主机与验收范围 | 结果 |
| --- | --- | --- |
| Windows x64 客户端 | 经校验的 ASIO SDK 2.3.4；portable 身份、隐私/许可证、CLI/UI 与固定安装 hash/隔离启动 | PASS |
| macOS arm64 客户端 | 已登录桌面的 Apple Silicon；结构、签名、原生启动和退出 | PASS，ad-hoc，非 Apple 公证 |
| Windows x64 服务端 | server/migrator 同源；显式 helper fixture、旧 updater 拒绝新布局、8/9/8 迁移回滚与隔离 health/库契约 | PASS |
| macOS arm64 服务端 | Apple Silicon；server/migrator 包与运行契约 | PASS |
| Linux x64 服务端 | Apple Silicon 上 OrbStack amd64 仿真；新格式 release 回归及包验收 | PASS，非物理 x64 主机 |
| Docker linux/amd64 | 消费同一已验收 Linux x64 包，仿真环境下离线镜像运行验收 | PASS |
| Docker linux/arm64 | 消费已验收内部 Linux arm64 包，ARM64 OrbStack 下离线镜像运行验收 | PASS |

唯一正式组装得到精确七个归档、七个同名校验文件；正式离线 verifier 的集合、名称与 SHA256 检查通过，54 份源绑定私有证明快照复制后身份匹配。Windows latest ZIP 只复制原字节为版本化名称，并重写校验文件引用的名称；所有归档字节保持不变。未把内部 Linux arm64 server 归档作为第八份公开包。

## 七份归档 SHA256

每份归档另有同名 `.sha256`，公开集合共十四项。14/14 线上 GitHub digest 与 size 均匹配本地已验收资产，7 份校验正文的正式文件名和摘要匹配下表；核验前后的 tag、Release 和资产身份保持不变。

| 正式归档 | SHA256 |
| --- | --- |
| `AudioLibExplorer-v0.9.4-win-x64-portable.zip` | `c121d968b71beb868f664a4574b50ab41b33b3997d4f2f4c75ed828c948acf3e` |
| `AudioLibExplorer-v0.9.4-macos-arm64.zip` | `a2a2992d6d571c431027be85b62e5ba606b58eb672dfd94edb00c964f7a7d87f` |
| `AudioLibServer-v0.9.4-win-x64.zip` | `d38dca3c9b39628b008dc32a50e4cbbfc002092ae521e53d16140a17b7228d23` |
| `AudioLibServer-v0.9.4-macos-arm64.tar.gz` | `aac10ce5c63a0fc95e6e47ba1976541292ffa2979192f77d86823b35682f7720` |
| `AudioLibServer-v0.9.4-linux-x64.tar.gz` | `7bf7d7d652068920bb4644b17ebc25f024b888ace877bd541a31c6eec16f8490` |
| `AudioLibServer-v0.9.4-docker-linux-amd64.tar.gz` | `40165d2a9207dae0fcc19038c8f54c1826c37b483e3752bbd9083f9ea3914f01` |
| `AudioLibServer-v0.9.4-docker-linux-arm64.tar.gz` | `1a9192e203ad9f0b09b78af9321fdca483dedfb92148c28a0eb75ad0c8af994d` |

## 覆盖限制与缓存维护

- 原始报告中的 2,005 个 WAV 和其他真实异常样本未纳入本轮验收，不能据合成回归宣称全部恢复入库。三个 ignored 中一个是主动导出跨实现 fixture 的辅助入口，另两个依赖真实 BOOM/SMED 样本。
- 未新增物理输出、主观音质、button-to-output 延迟或 ASIO 硬件测量。数字解码、状态、Range/SHA256 与无 underrun 记录不替代这些测量。
- beta 原有 909,342 条记录中，已配置 root 覆盖 80,562 条，另外 828,780 条是配置覆盖范围外的历史保留记录；受保护 stream 返回 404 是该 beta 既有配置边界，不是此次丢失证据、逐文件读权限审计或所有用户的产品限制。
- Windows 已登记并经真实 Cargo 标记校验的 x64 子 target 完成 Debug 40 GiB 预算维护：保留 9,960,093,082 bytes（约 9.276 GiB），删除 0，budget_met=true。父根因无真实标记而在枚举前安全拒绝，原记录保留；host profiles 只读估计 6,039,883,025 bytes，合计 15,999,976,107 bytes（约 14.901 GiB），不冒充完整维护通过。未登记的 Mac/Linux 历史缓存继续保留，旧 beta 失败记录不改写。

## 远端核验与发布后安装

发布后只读核验确认正式标题、非 draft、非 prerelease、latest、远端 annotated tag 及其 f6 peeled commit，正式版本与资产两项工具检查通过；精确十四项资产的 GitHub digest/size、七份已下载校验正文通过，无归档下载 fallback。核验前后的 tag、Release 与资产身份未变化，线上资产与已验收字节一致。

随后使用这一成功的远端证明，将同一已验收 Windows payload 与原固定 `local_release/` 同步：唯一一次实际 adapter/隔离启动调用通过，copied=0、verified=908，两个 EXE 身份、CLI 0.9.4、桌面启动及正常数据目录环境恢复均通过。此前两次 wrapper 只读合同诊断未调用安装或启动，其记录保留；实际同步未重建、未重跑 portable 套件、未改变正常库与偏好。

原始源码、打包、原生验收、发布后远端与安装证明保留为私有证据，不上传为资产。后续文档同步不移动既有 tag、不覆盖 0.9.3 历史资产、不重建已验收 payload；文档后的 main HEAD 不冒充七份二进制的 f6 源码身份。
