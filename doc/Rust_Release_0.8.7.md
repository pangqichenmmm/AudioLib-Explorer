# 0.8.7 发布准备跟踪

记录日期：2026-11-02。本文记录范围整理与后续门禁；“准备完成”不表示候选或正式产物已验收。

## 身份与范围

| 项目 | 本轮基准 |
|---|---|
| 已发布基线 | [v0.8.6](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.8.6)，提交 `9b883f9c6c35886682ec3e1df1dbd40720c25339` |
| 整理时 beta | `20455819ea15b642a33f779f6d83abde60a13d36` |
| 目标程序版本 | 0.8.7，PATCH；正式 main/tag 提交待确定 |
| 本机候选应用身份 | beta `86acdac21468ac0f88228f5a4b613ad49c8eaa31`，合并 [#194](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/194) |
| beta 服务验收身份 | `b1e428cc4ceec2f3993b3fdc859f8227f18660d9`；后续文档/流程提交保留该服务与既有客户端的原始证据身份 |
| API / schema | `/api/v1/` / 8；快照兼容 `3..=8` |
| 完整格式 | `ale-s8-cld1-t3-m2-p1-a1`，无新增数据库迁移 |
| 正式资产 | 七个归档及各自 SHA256，共十四项，沿用现有命名契约 |

本轮范围包括已合入 beta 的内容，以及用户确认追加到本候选的 30 任务功能：

- 恢复原 Default 外观，注册可选 Studio 工作台；主题切换保留共享播放、Waveform 与浏览状态。
- 修正 Studio 输入、静音确认、视图恢复与过期回调的竞态，保留原生键盘浏览行为。
- 修复 Windows Common Controls 加载依赖。
- 缩小播放重绘区域，缓存 Studio 表面位图，保留真实窗口露出时的完整重绘；恢复细播放头并完善播放/暂停反馈。
- 本地 beta 使用开发身份并隔离自动更新；发布管线支持续跑、产物身份校验、固定安装和预算缓存维护。
- 搜索任务上限从 6 提高至 30，新安装默认 30；已保存的个人上限（包括 6）保持原值，可在外观设置中调整。达到上限不替换已有任务，下调到低于当前任务数时要求先关闭任务。
- Default / Studio 的任务栏在溢出时提供左右翻页箭头，激活或新建任务时自动显示对应标签；`+` 固定在滚动区旁。个人任务、收藏及恢复仍使用客户端工作区，不增加后台并行检索数。
- 工作区格式版本保持 2；旧客户端仍拒绝超过 6 个任务的工作区，并进入保留原文件的只读回退，避免降级后覆盖丢失任务。此保护基于校验，不是资料库 schema 或 API 变更。
- 区分普通搜索任务的视图恢复与主题工作台切换的恢复所有权，修正结果追加和远程/本地模式切换中的滚动回退。结果列表均保留两阶段恢复以保护小数行锚点，待恢复坐标按实测范围收敛；普通面板在首阶段应用，工作台面板在最终阶段应用，旧来源取消保护保持有效。相关测试、断言和等待时序未修改。
- 原生重绘工具新增可选 `-Theme default|studio`，按 `<theme>-<nonce>` 分开保存每次报告；精确选择应用窗口，验证遮挡、焦点转换与正常关闭，保留失败历史。

这些内容来自已合并的 [#186](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/186)、[#188](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/188)、
[#189](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/189)、[#190](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/190)、
[#191](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/191)、[#192](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/192)、[#193](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/193) 和
[#187](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/187)。30 任务功能来自原工作区，在首轮版本准备后获用户确认追加，并经 #194 合入 beta；所选源码、本机 portable、两主题原生窗口和 beta 两端验收已通过，最终 main/tag 与正式产物门禁仍待完成。行为说明见[主题契约](Rust_Themes.md)、[搜索任务](Rust_Search_Tasks.md)与[发布说明](Rust_Release_Notes.md)。

## 暂不纳入的工作

- [#185](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/185) 与已合并 Studio 外观部分重叠，但并非全部被取代：其独有的分页、结果数量、侧栏显示偏好和 draft 上传 helper 延后单独评估，不直接合入冲突分支。
- [#135](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/135) / [#137](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/137) 的封面工作已由合并的 [#138](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/138) 接续；[#97](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/97) 的 marker 方向已有后续实现。这三个过时 PR 已关闭，保留分支与提交；[标记设计](api/Compatibility_Markers.md)已注明历史状态，未合入旧草稿代码。

## 已有证据与未解决门禁

以下为原 PR 的历史记录，未在本次准备中重新执行；只有相关源码、feature、工具链及环境仍相同时才可复用。

| 原始身份 | 已记录结果 | 当前限制 |
|---|---|---|
| #193，集成提交 `db5b31e` | 17 项最终定向回归、格式与 desktop strict Clippy；默认 ASIO 构建、portable、播放/IPC、固定与 beta 安装通过 | 早于 #187，不能用其二进制充当最终 0.8.7 payload |
| #187，提交 `6506013` | Windows desktop 469 passed / 4 failed；Windows crate 27 passed；格式、相关 Clippy、loader 测试通过 | 四项结果滚动测试仍失败；旧 beta `e3a3bde` 同样失败并不使它们成为通过 |
| #187，Windows 非 ASIO 测量 | 局部播放重绘、最小化恢复与缩放有记录 | 未执行 ASIO 打包及固定安装；锁屏使完整原生窗口露出验收中断，焦点/遮挡尚未完成 |
| v0.8.6，`9b883f9c6c35` | 正式发布及十四资产记录 | 仅证明旧版，不能替代本轮候选或最终身份 |

本轮 ASIO 运行已重现四项滚动失败：`result_paging_tests::appending_rows_keeps_the_visible_content_and_other_pane_scrolls_in_place`
及 #187 报告中的三项 `mode_switch_scroll_tests`。首轮 desktop 全量不再出现这四项失败，但发现两项 Studio 小数行锚点失败；补齐两阶段恢复后，最终定向复验已覆盖并通过这六项及相关恢复保护，原断言和等待时序未改。
在可交互、未锁定的 Windows 桌面完成两主题的原生焦点、遮挡、恢复与重绘验证；此前锁屏记录不算通过。

## 本轮集成验证进展

源码身份为 `fa9c53abe9287595823135f9280f8e43237be9e3` 加本轮集成差异；原始补丁、收据及日志保存在私有目录 `.rust-validation/release-087/integration/`。`source-receipt.json` 与 `validated-source.patch` 保留首轮已检输入；`source-receipt-final.json` 记录第二次视图恢复修正后的输入。core/db/remote/windows/update 源码未变，复用其通过结果；desktop-only strict Clippy 与受影响测试已单独复验通过。

| 检查 | 当前实测结果 |
|---|---|
| 格式与选定 strict Clippy | 首轮 fmt 与 core/db/remote/desktop/cli/windows/update 的 all-targets、Rust 1.97.1 MSVC、客户端 ASIO 检查通过；第二次 UI 修正的 desktop-only strict Clippy 亦通过，日志 `clippy-ui-final.log` |
| core | 142 passed，0 failed |
| db `search_workspace` | 10 passed，0 failed |
| remote 离线个人工作区消费者 | 1 passed，0 failed；其余测试未由这个定向命令执行 |
| windows | 27 passed，0 failed |
| update | 29 passed，0 failed，2 个 helper 用例 ignored，不计为通过 |
| 滚动失败复现，修复前 | ASIO：22 passed / 4 failed / 2 ignored；确认为既有四项失败，日志 `scroll-before.log` 保留 |
| 首次修复后 desktop 全量 | 471 passed / 2 failed / 154 ignored；失败原日志 `desktop.log` 保留，不计为全量通过 |
| 第二次修正的原因 | `studio_switch_review_tests` 的 `explicit_end_key_selection_supersedes_a_pending_workbench_restore` 与 `pending_theme_restore_uses_the_current_font_and_scale_row_height` 在第 83 行 fixture 小数行锚点断言失败；修复恢复逻辑，测试/断言/等待时序未变 |
| 第二次修正定向复验 | 106 passed / 0 failed / 51 ignored / 470 filtered，216.47 秒；ignored 为由父测试调用的 helper，不重复计为通过。覆盖原两项 Studio 锚点、四项滚动、旧工作台取消保护、30 任务/模型/缩放/主题与播放重绘；日志 `desktop-ui-final.log`。这是定向结果，不称为新一轮 package 全量通过 |
| Mac arm64 源码检查 | 提交 `053c2a2c5497ce68447a558792ad6c97dd4b7a62`，Rust 1.97.1、`aarch64-apple-darwin`、默认 features / 无 ASIO：desktop all-targets strict Clippy 通过；相关定向测试 106 passed / 0 failed / 51 ignored / 467 filtered。未执行 release 构建、服务端检查或部署；私有收据 `.rust-validation/release-087/mac-source/native/mac-source-20261102T064032Z-ccad82fa/result.json` |
| 实际 payload 与原生重绘 | beta `86acdac21468` 的 Windows portable 已验收并安装；后续修正验收工具后，两主题原生重绘均通过，详见下节；未重建应用 |

## 本机候选交付与原生门禁

以下实际产物均来自 beta 应用提交 `86acdac21468ac0f88228f5a4b613ad49c8eaa31`；短阶段收据和原始日志位于私有目录 `.rust-validation/release-087/delivery/`，不在本文嵌入完整日志。

| 阶段 | 当前结果 |
|---|---|
| Windows 默认 ASIO release | 构建通过，10 分 27 秒；打包许可证、旧更新器兼容及隐私检查通过 |
| 本机实际 payload | portable、`validate-review-fixes.ps1 -CleanCheckout` 和 playback/IPC 均通过 |
| 固定安装 | `X:/AI_Build/AudioLib_Explorer_oss/local_release/` 安装器通过版本、1,249 个包文件 SHA256 与一次隔离启动；13 个文件内容更新，不触及日常库和偏好 |
| 独立 beta 开发安装 | `-Development` 安装已完成，1,249 个包文件，与固定安装使用同一已验收 payload |
| Default 原生重绘 | 首次基线检查及后续 refocus 失败记录保留。修正工具后通过：192 DPI（200%）、2214×1370 client，失焦/遮挡/最小化恢复各三轮，共九轮，采样 `changed_fraction=0`；`foreground_exposure_verified=true`、`normal_shutdown=true` |
| Studio 原生重绘 | 通过：192 DPI（200%）、2214×1409 client，同样九轮、采样 `changed_fraction=0`，前台曝光与正常关闭均为 true |
| Mac beta 服务门禁 | `b1e428c` 构建、替换及 health 已通过；首轮根读取超时失败保留，同一 payload 的只读阶段续验确认唯一配置根为 206 / 4,096 字节 / WAVE，前后 health 均为 0.8.7 / `b1e428c` / ok / API v1 / schema 8 |
| Windows beta smoke | exit 0，10 passed / 0 failed，20/20 配置根内流样本通过；强制全量同步 909,342 条，与服务端相等；快照 10,150.7 MiB，同步 147.2 秒，总计 160.38 秒 |
| 最终多平台产物 | 尚未确定最终 main/tag 身份；beta 两端与 Mac 源码检查不替代七个归档及十四资产的正式构建和验收 |
| 缓存维护 | 失败诊断完整保留；尚未执行预算清理 Run。成功交付后的预算维护仍待执行；`C:/audiolib-build/target` 未注册，保持原样 |

首轮只将基线图保存移到断言前，仍失败；后续定位并修正的是验收工具，未修改应用 Rust 源码或重建应用：

- `Process.MainWindowHandle` 可能选中同进程的 `Winit Thread Event Target` 工具窗口。捕获该窗口会误报无可见控件，清理时误发 `WM_CLOSE` 还会销毁事件循环的唤醒目标，使已排空的真正应用隐藏后仍无法退出。现在按可见、同 PID、精确标题和类名选择唯一应用窗口，在正常关闭与失败清理前复核身份。
- WinForms 遮挡窗的托管 `TopMost` 状态不足以保证原生样式持续有效；在创建 HWND 时显式设置 topmost/toolwindow，按物理坐标覆盖 client 并留出圆角余量，通过四角和中心命中测试验证真实遮挡。
- 最小化后使用自有备用窗口取得前台，短暂绑定两个自有测试队列请求切换，并在 finally 解绑定，不依赖或操作用户的其他前台窗口。仍要求实际最小化、备用窗口获得前台、应用恢复前台及原生像素保持，未降低内容或差异阈值。

两主题均使用同一旧 payload：`AudioLibExplorer.exe` SHA256 为 `4bd1265d2c31c5e6856c60732e495ca74ffa3b25f33f05a10f49d7508fec3061`。最终报告分别为私有路径 `.rust-validation/window-repaint/default-8ab756a0305f4807afedce875d918e33/report.json` 与 `.rust-validation/window-repaint/studio-ad83e26572f74a069fb6c0eef99b9e08/report.json`。较早的 Default 通过报告 `default-8773ae723630414d8e4f70767afa4ca5/report.json` 也保留；最终复验补齐了相同的逐轮结果字段，未覆盖旧报告。历史失败日志与原始 delivery 阶段快照仍保留，不改写为通过。

安装器及 playback 失败清理同步改用精确应用窗口选择器，无真实应用窗口时阻塞并保留进程和数据。已有安装合成回归及复用此 payload 的临时安装回归通过：1,249 文件身份一致，两次安装的复制数分别为 1,249 和 0，隔离启动、原有文件与偏好保留；日志 `.rust-validation/native-validation-087/installer-runtime.log`。这些检查只操作回归自有临时安装，不再次覆盖固定安装。

## beta 两端验收与阶段续跑

Mac beta 服务以 `b1e428cc4ceec2f3993b3fdc859f8227f18660d9` 构建，耗时 65.13 秒。原 refresh 耗时 18.78 秒：替换与 health 成功，但首个配置根的 15 秒读取超时，原始退出码 1 和失败日志完整保留，不改称首次 refresh 通过。

随后同一根的同一 WAV（记录 ID 446）恢复可读：终端读取 0.437 秒、API Range 读取 0.008 秒，均完整取得 4,096 字节并识别 WAVE。超时后恢复的原因未证实，不能归因为已证实的 TCC 权限或磁盘唤醒问题；本轮没有为通过测试扩大访问权限或修改既有 beta 配置。

独立只读阶段续验核对本地 HEAD、`origin/beta` 和实际 beta 服务均为预期提交，已冻结与安装的 server SHA256 均为 `b4a3433f75a4a488815e4b5860da192dde58bdfe5b55c741cdefae1af58c831f`，launcher、run 脚本、plist 与 config 四项哈希保持不变。唯一配置根的 Range 读取为 206、4,096 字节及有效 WAVE 头，前后 health 身份一致；阶段 exit 0。服务门禁结论来自原构建/替换/health 加只读根阶段续验，不是重新运行或覆盖原失败 refresh；没有再次构建、替换、重启，也未触及生产实例。私有收据为 `.rust-validation/release-087/beta-refresh/runs/beta-b1e428cc4cee-20261102T073546Z-040e1086/stage-resume.receipt.json`。

Windows 使用既有客户端应用提交 `86acdac21468ac0f88228f5a4b613ad49c8eaa31` 的 Development beta 安装，对预期服务提交执行完整 smoke：10 passed / 0 failed，20/20 流样本通过，强制全量同步的 909,342 条与服务端一致。结果与私有原始日志索引见 `.rust-validation/release-087/beta-refresh/beta-smoke-result.json`。

覆盖范围必须保留：资料库共 909,342 条，既有 beta 配置的一个根覆盖 80,562 条；其余 828,780 条可浏览 Metadata，但位于配置根外，`/stream` 返回 404。smoke 的流样本仅从配置根内选择，跳过了 172 个根外候选；这不是全资料库可播放验收，也没有通过修改配置或扩大权限掩盖该边界。

本次追加只涉及文档/流程证据，复用 `b1e428c` 服务和 `86acdac21468` 客户端的原始身份；相关应用输入未变时不为文档提交重复构建或 refresh。最终 main/tag 的七个正式归档及十四资产仍须按最终身份构建和验收，不能将上述 beta 产物直接记为已验收的正式 payload。

## 按实际影响执行的后续计划

1. 同步 workspace 与十个本地包的锁文件版本、README、版本规则及本轮发布说明；保留旧标签和历史证据身份。
2. 运行版本预检及文档差异检查。首轮版本准备仅执行这些轻量检查和管线计划预览；追加 30 任务后的所选源码、交互、持久化、本机交付及 beta 两端验证已按上表补齐，最终产物另行验收。

   ```powershell
   pwsh tools/check-release.ps1 -Tag v0.8.7 -BaseTag v0.8.6
   git diff --check
   ```

3. 对最终候选按 [Testing Strategy](Testing_Strategy.md) 选择源码检查。覆盖 core 的主题配置、1..=30 上限和工作区校验；db 的工作区往返、超限保存保护；remote 的客户端个人工作区消费者；desktop 的任务创建/恢复/收藏/限额、双主题任务栏与既有输入/滚动/Waveform/重绘/更新策略，以及 Windows 加载与露出、update 开发身份。公共接口检查相应直接消费者。Windows 客户端使用一次默认 ASIO 配置，先按[仓库规则](../AGENTS.md)准备并核验 SDK；不串行追加整套非 ASIO 矩阵。
4. 30 任务回归须核对重开后的顺序、最后活动任务与视图，保留旧个人上限、降级拒读时原工作区不被覆盖，以及两主题/语言/支持缩放下的箭头、固定 `+` 和活动标签显示。配置持久化与工作区保存选择相关数据保护回归，并执行受影响的 [review validator](../tools/validate-review-fixes.ps1)；关闭排空或 Metadata/扫描行为变化时再补对应专项，保留仍有效的证据。播放与 IPC 变化使用 [playback validator](../tools/validate-playback-ipc.ps1)。
5. 版本本身不触发全 workspace 或百万记录 benchmark。重绘风险使用 [window validator](../tools/validate-window-repaint.ps1) 的 `-BinaryDirectory` 参数及相关播放重绘测试；分别传 `-Theme default`、`-Theme studio`，报告写入 `.rust-validation/window-repaint/<theme>-<nonce>/report.json`，以输出的 `report_path` 为准。报告须确认焦点/露出真正执行；性能结论保留实际平台、分辨率、构建配置和采样条件。本轮实测为 192 DPI，不扩展声称 100% DPI 已完成原生验证。
6. release pipeline、beta 安装或包规则若无可复用同源证据，分别执行已有 synthetic 回归；不修改规则时不重复包边界测试。
7. 准备 beta 候选时使用独立 beta 数据目录和开发安装身份；由 Mac beta refresh 与 Windows [beta smoke](../tools/beta-smoke.ps1) 对同一预期服务提交验收。本轮已按上述阶段续验和完整 smoke 通过，保留原服务与客户端各自身份。服务地址和凭据仅从本机环境提供；后续相关应用输入未变的文档/流程提交复用这些证据，不重复部署。
8. beta 通过后才按 [版本流程](Rust_Versioning.md) 提升 main。最终 main/tag 身份确定后，各原生平台构建、打包与验收各一次；Windows client 优先使用 [release pipeline](Rust_Release_Pipeline.md)，不要重复调用其内部阶段。
9. Windows 实际 payload 通过一次 portable 验收后更新固定 `local_release/`，核对全部 package-owned SHA256、CLI 版本及一次隔离启动；保留 `%LOCALAPPDATA%/AudioLibExplorer/v1` 的库、Metadata、偏好与缓存。beta 安装不能替代固定安装。
10. Mac 客户端与各服务端/Docker 包按本机计划及实际变更选择检查，记录未覆盖平台；最终正式包须具有同一发布提交身份。成功交付后执行一次 40 GiB debug 缓存预算维护，报告阻塞或未达预算。

## 当前状态

- 已完成：beta/已发布基线核对、范围划分、原始证据与缺口清单，0.8.7 版本/锁文件/发布说明同步、过时 PR 关闭及历史设计状态校正；用户随后确认将 30 任务功能纳入本轮。
- 首轮版本准备实测通过：`check-release.ps1 -Tag v0.8.7 -BaseTag v0.8.6`、`git diff --check`；Windows client/server、Unix server、Mac client 四种原生 release plan 的 `Plan` 路由预览通过（`source_checks=not-requested`）。这是追加 30 任务前的原始记录，计划预览不运行构建、源码测试或平台验收。
- 源码检查已通过：最终 desktop strict Clippy 与定向 106 passed / 0 failed / 51 ignored，结合未变 core142、db 工作区10、remote 离线工作区1、windows27、update29 及既有格式/Clippy 结果，完成本轮所选源码验证。保留各输入与 ignored 限制；首轮 desktop 的 471 passed / 2 failed / 154 ignored 仍是失败历史，未改称全量通过。
- Mac 源码检查已通过：`053c2a2` 的原生 arm64 desktop strict Clippy 与所选 106 passed / 0 failed / 51 ignored；不代表 Mac release payload、服务端或部署通过。
- 本机交付与原生门禁已通过：beta `86acdac21468` 的默认 ASIO 构建、打包、portable、review-fixes、playback/IPC、固定 `local_release/` 与独立 Development beta 安装；同一 EXE 的 Default / Studio 原生曝光各九轮通过并正常退出。保留所有先前失败及工具修正记录，未另行重建应用。
- beta 两端已通过：Mac `b1e428c` 的原构建/替换/health 与只读根阶段续验结合通过；Windows `86acdac21468` 客户端完整 smoke 为 10 passed / 0 failed、20/20 流样本及 909,342 条全量同步。原根读取超时失败和配置根覆盖边界保留，恢复原因未证实。
- 尚待完成：最终 main/tag 身份、七个正式归档及十四资产的构建/验收/发布流程，以及交付后的缓存预算维护。现有候选与 beta 证据不表示正式 payload 已验收或 Release 已发布。
- 保留全部失败证据与诊断缓存；本次文档/流程追加复用仍有效的原服务、客户端身份，没有把旧构建或历史测试描述为新执行。

门禁结果在实际执行后追加其提交、feature、平台与私有报告路径；不得将本文件的计划表当成通过记录。

本轮本机汇总：固定安装验证 1,249 个 package-owned 文件（13 个内容更新），CLI 为 `AudioLibCli 0.8.7`，隔离桌面启动通过；`beta_release/` 的 Development 安装也已完成（1,249 文件）。两处安装使用同一已验收的客户端 payload，工作资料库与数据目录保留。

缓存维护仅执行 Plan：首轮预览时已拥有的项目 target 约 **66.65 GiB 逻辑大小**，未执行删除，不代表当前预算已达标。Plan 中的 `remaining_bytes=0` / `budget_met=true` 是执行所列删除后的预测，不能作为实际回收记录。预算 Run 待本轮交付后执行；原生失败诊断及未注册的 `C:/audiolib-build/target` 均保留。

原生失败后，一个 owned `--smoke-view window-repaint` 测试进程在常规关闭后隐藏窗口但未退出。确认其可执行路径、专用 smoke 参数、隔离空库路径及 Metadata/扫描已排空日志后，仅对该超时测试实例执行了回收；保留临时数据与 `native-timeout-cleanup.json`。该次回收仍不是正常关闭通过记录；后续已从工具窗口误选定位其失去唤醒目标的原因，并以精确窗口选择和两主题真实正常退出完成本轮复核，未把该现象归为新的 Rust 关闭排空故障。

最终产物准备时修正 Windows client 计划的归档声明：打包器常规输出为 latest ZIP/SHA256，计划现在记录这两个实际文件。Profiles 合成回归保留真实产物声明，验证错误的版本化声明会在验收和安装前失败，以及修正后两种 ASIO 路由通过；应用源码、包格式与白名单未变。私有日志为 .rust-validation/native-validation-087/pipeline-profiles-final.log。
