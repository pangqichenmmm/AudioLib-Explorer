# 本地 Release 管线

`tools/release-pipeline.ps1` 用一次调用串联已有脚本，要求 PowerShell 7.3+。默认只展示计划；显式 `-Action Run` 才执行。原有构建、许可证、隐私、启动及数据安全检查保持各自职责，不在执行器中复制一套。

## 常用调用

```powershell
# 编辑期间预览；不构建、不验收、不创建运行目录。
pwsh tools/release-pipeline.ps1 -Tag v0.9.0

# 在已创建、指向当前干净 checkout 的实际发布标签上运行。
# Windows 客户端：版本/许可证预检 → ASIO 构建 → 打包 → portable 验收 → 固定安装。
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.9.0

# 原命令再次执行即续跑，成功阶段先核对输入、环境和产物再复用。
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.9.0

# 只读取短状态，不执行 Git/Cargo 或重新验收；这是上次运行的快照。
pwsh tools/release-pipeline.ps1 -Action Status -Tag v0.9.0

# 失败时只查看指定阶段的日志尾部，每个日志最多 80 行。
pwsh tools/release-pipeline.ps1 -Action Logs -Tag v0.9.0 -Stage accept -Tail 15 -MaxCharacters 6000
```

每阶段开始/结束各输出一行 JSON，长阶段每 60 秒补一行状态，最后输出阶段摘要和 `state.json` 路径。完整 stdout/stderr 分开落盘，每次尝试保留独立日志和收据，续跑不覆盖原失败证据；根目录日志是便于下游阶段读取的最新副本。不要把整个日志、安装文件清单或历史验证报告读进对话。使用工具运行长管线后，等待同一会话的增量结果，通常每次等待 30–60 秒；无需对每个子命令再发工具调用。

记录默认位于 `.rust-validation/release-pipeline/<tag>/<plan-name>/`，属于私有、本机生成证据，不得作为 Release 资产。`Status` 不重新证明当前输入有效；只有 `Run` 才执行复用判定。日志查看同时限制行数和 stdout/stderr 的合计字符数，单行巨型 JSON 也不会整段进入工具输出。失败保留日志、当前阶段状态和其他有效的成功收据；失败阶段自身的旧收据失效，不继续后续阶段。进程中断后同样通过 `Run` 重试。运行目录和子阶段各有独占锁；父执行器中断但构建/安装子进程仍在运行时，重试会被拒绝，等待子进程退出后再续跑，不强制终止它。

## 选择范围与配置

| 计划 | 平台与职责 |
| --- | --- |
| `tools/release-plans/windows-client.json` | Windows x64 客户端，默认 ASIO，包含固定 `local_release/` 安装 |
| `tools/release-plans/server.json` | Windows x64 无播放服务端，显式构建验收 fixture 所需 benchmark |
| `tools/release-plans/server-unix.json` | 对应本机 Linux/macOS 服务端，不额外构建 Windows fixture helper |
| `tools/release-plans/macos-client.json` | Apple Silicon Mac 客户端，结构与原生验收合并运行；需要已登录桌面 |
| `tools/release-plans/release-assets.json` | 只读获取已发布资产元数据，核对精确资产集合和本地归档 SHA256；使用已有 GitHub 凭据 |

Windows 客户端计划按打包器的实际常规输出记录 `dist/latest/AudioLibExplorer-latest-win-x64-portable.zip` 及其 `.sha256`；`-Tag` 写入包的版本身份，不改变这两个本地文件名。

四个原生计划在 release 构建前执行 `license-preflight`，复用 `dependency-licenses.ps1` 的选定依赖图与 notice 解析，检查实际 target、roots、features 及固定材料身份。此阶段只使用 locked/offline Cargo 元数据，不编译、不下载、不生成包。它每次 Run 都重新检查外部依赖缓存中的材料；输入不变时收据身份稳定，不使后面的成功构建失效。打包与验收仍执行原有许可证检查，ASIO SDK 等额外材料仍由原准备/打包流程验证。离线缓存不完整时先准备依赖，不通过一次试编译发现缺件。`-ChecksFile` 的源码检查仍在版本预检之后插入；若本轮还需执行耗时源码检查，在前期环境核对时先单独运行下面的轻量许可证预检。

候选构建若不经过标签管线，也应先调用同一预检入口；服务端通过不能替代客户端闭包通过：

```powershell
pwsh tools/check-release-licenses.ps1 -Profile WindowsClient
pwsh tools/check-release-licenses.ps1 -Profile WindowsClient -WithoutAsio
pwsh tools/check-release-licenses.ps1 -Profile MacosClient
pwsh tools/check-release-licenses.ps1 -Profile Server -Target aarch64-apple-darwin
```

预检在相应构建主机上执行；Mac/Linux 的闭包和后续原生验收仍需匹配主机，不将 Windows 合成路由测试描述为原生通过。

其他计划通过 `-PlanFile` 选择。用 `-VariablesFile <private.json>` 覆盖该计划声明的变量；保留变量 `Tag`、`Commit`、`Root`、`RunDirectory`、`TargetDirectory`、`Asio` 不允许覆盖。`Asio` 由 `WithoutAsio` 自动派生，未声明 `WithoutAsio` 的计划为 false。例如 Windows 无 ASIO 配置为 `{"WithoutAsio":true}`，对应的 build/package/accept 和示例源码检查会同时改变；不追加另一套矩阵。Linux arm64 服务端为 `{"Target":"aarch64-unknown-linux-gnu","Suffix":"linux-arm64"}`，Mac 服务端为 `{"Target":"aarch64-apple-darwin","Suffix":"macos-arm64"}`，必须在匹配主机运行；仿真执行仍须明确记录。不同平台/特性使用独立运行目录。

源码检查按实际改动选择，不因为管线名称包含 release 就全量重跑。将选好的阶段写入私有 `checks.json`，通过 `-ChecksFile` 前置执行；`tools/release-plans/checks.example.json` 仅示范受影响 Windows 客户端的单次 ASIO 检查。不要直接把示例当每次发布的必选清单。专项数据安全回归可作为额外阶段调用原脚本，参数与原入口一致。未传 `-ChecksFile` 时摘要明确记录 `source_checks=not-requested`，不宣称功能测试通过；已在别处完成的验证保留原身份，按 Testing Strategy 判断有效性。

```powershell
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.9.0 -ChecksFile .rust-validation/checks.json
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.9.0 -PlanFile tools/release-plans/server.json
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.9.0 -PlanFile tools/release-plans/server-unix.json -VariablesFile .rust-validation/linux-arm64.json
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.9.0 -PlanFile tools/release-plans/macos-client.json
```

Mac 构建失败的原始证明保留为独立 attempt 文件；成功后才更新供打包使用的证明。Mac 正式归档不可覆盖：已成功归档后来失效时，选择新的运行目录重新交付，不删除旧验收证据。Windows 安装器先核对完整包白名单、版本和所有目标路径，再关闭应用和复制文件；安装过程有独占锁，拒绝重叠来源路径。它缓存已经确认过的目录祖先检查，跳过内容相同的文件，仍核对所有包文件 SHA256、CLI 版本和一次隔离启动。锁定文件或未排空的实例使安装失败，不强杀进程，不清空安装目录和工作数据。相同安装产物的有效管线收据可直接复用。

## 复用与扩展

计划是可信的可执行配置，不能运行来自不受信来源的计划。每个阶段指定唯一 `id`、`script` + 类型化 `parameters`，或原生命令 `command` + `arguments`。参数通过 JSON 传给独立 PowerShell 子进程，不拼接 shell 命令；布尔值和数组保留类型。变量使用 `${Name}`，完整变量值可为布尔或数组。`needs` 只能引用之前的阶段。

GitHub 阶段可声明 `github_auth: true`：依次使用已有 token 环境、`gh` 缓存凭据、Git Credential Manager 的非交互凭据。token 只进入该阶段子进程的环境，不写入参数、状态或父环境，也无需每次工具调用重新拼接认证代码。没有凭据时失败，不弹出登录流程。

- 默认按确切源码提交复用，适合构建、打包、实际 payload 验收。最终 main/tag 身份变了就重新生成实际交付物。
- `reuse: inputs` 适合源码功能检查，必须显式声明 `input_paths`；使用 Git 对象清单验证相关输入，不重新读取或哈希整套源码。文档提交可保留未变源码检查，但不能让不完整的路径列表掩盖真正改动。
- `input_files` 对 Git 外的原生包、验收报告或其他文件/目录计算身份；`outputs` 声明必须存在且匹配 SHA256 的产物。目录包含完整相对文件集合，并拒绝链接；Unix 同时核对文件权限模式。
- `needs` 引用的已验收产物保持只读；每次实际阶段执行后重新核对依赖身份，修改它们会使当前阶段失败并阻断安装。需要变换产物时写入本阶段的独立输出。复用前和最终汇总前同样重新确认 checkout/标签身份。
- 源码、执行器、命令脚本、参数、依赖收据、工具版本或已声明环境变化，都会使相关阶段失效。标准的目标编译器参数、Cargo target/profile、SDK 和链接环境自动计入身份；工具版本在仓库目录中查询，匹配实际 Rust 工具链选择。外部 SDK/工具文件变化时也应使用新运行目录；额外相关环境键可放入 `environment_variables`，工具版本放入 `environment_tools`。
- `reuse: never` 用于必须重新读取的在线状态或外部可变材料（如 registry notice）。状态、日志、命令参数文件均私有；token、密码和服务器地址通过本机环境提供，不写入计划或发布记录。

Docker 继续通过 `package-server-docker.ps1` / `validate-server-docker.ps1` 消费已验收的 Linux 包。其本机路径与 fixture 需明确选择，可按上面的阶段格式放进私有计划；不要把 Docker 验收与原生包验收互相替代。不同本机平台各调用一条计划，执行器不假设 SSH 主机、生产配置或共享容器。

## 多平台协调与失败恢复

先固定本次请求的 payload 范围及候选，beta 门禁通过后再冻结最终 main/tag 身份。用简短表格记录每份 payload 的 target、features、主机、运行目录、原有证据和剩余门禁；现有管线状态继续承担阶段记录，无需每轮重写 `.rust-validation` 中的历史编排器。

- Windows、Mac、Linux 在独立构建/输出目录上并行；共用 Cargo target 或同一个 `dist/latest` 子树的任务顺序执行。Mac 客户端和服务端同样需要避免共享 target 上的编译争用。
- Linux x64/arm64 共用 `server-unix.json` 的 `unix-server` 计划名。两者显式使用不同 `-RunDirectory`，并核对 Target/Suffix、构建缓存及输出隔离，不能只换 VariablesFile 后沿用默认运行目录。
- 某架构 Linux 包验收通过即可启动该架构 Docker，无需等待其他平台。Docker 导出/压缩失败可使用已有 `-ExistingImageId sha256:...` 恢复；入口仍会检查原生包和镜像内二进制身份。
- 同一 Run 负责判断哪些收据可复用。打包失败时保留构建，验收驱动失败时保留实际 payload；输入变化导致管线判定失效时按其结果执行，不删除状态或手写通过收据。新预检改变了计划身份，首次使用新版计划时已有旧收据可能失效，不承诺跨这次工具更新复用历史构建。
- 上传失败先检查已有 draft/Release 和完整资产，保留已完成的传输与原字节。大 Docker 归档优先从持有已验收文件、网络可用且已有授权凭据的主机上传，避免经另一台电脑中转 NAS 大文件。继续使用正式发布入口；`release-create.sh` 当前没有通用续传或替换开关，不虚构参数。
- 发布完成后的说明/验收记录更新不触发重建。保留实际源码和二进制身份；同版本替换仅限明确授权的 payload，不能移动 tag，也不能顺带重建、上传其余包。普通标签计划要求 tag 与 checkout 相同；替换使用明确的独立流程和真实来源证明，不能伪造标签身份。例见 [0.9.3 客户端替换记录](Rust_Release_0.9.3.md)。

发布后如果固定安装已与正式 payload 全文件哈希一致，按 AGENTS.md 完成发布身份和安装核对即可；不因“已发布”再跑一遍完整 portable 套件。生产服务更新另按请求授权和部署契约执行。

## 近期交付复盘（2026-11-09）

本轮对照了“将 beta 合并到 main 并发布小版本”“整理 beta 分支并启动下个版本发布”“同步 beta 并准备下一版本发布”“排查预览播放指针残留与破音”“精简开发与发布测试流程”的消息与命令记录，以及仓库 0.8.7、0.9.0–0.9.3 发布记录。聊天总时长包含审查、修复、等待和上传，不等于打包耗时；以下只列有阶段证据的事件。

| 事件 | 效率结论与处理 |
| --- | --- |
| 0.9.3 Mac 客户端 build 成功耗时 533.4 秒，随后 package-accept 在 3.76 秒因 `block2@0.5.1` notice 缺失失败；补查实际闭包为 492 项，10 项缺件 | 新增构建前精确闭包预检。0.9.0 的 Mac server 材料已补齐，仍未覆盖此 client 闭包；不能以另一种产品的预检替代。私有原始证明：`.rust-validation/release-093/mac-candidate/native-package-failure-inspection.stdout.json`、`native-selected-license-preflight.stdout.json`。 |
| 只读远端核对时，[v0.9.1](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.1) 和 [v0.9.2](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.2) 均各有 Windows client、Mac server 两归档及两份 SHA256，未达到当前完整发布的七归档集合；本机未找到这两版的阶段验收收据 | 不以“已发布”推导全平台交付或验收通过，也不把少发平台当默认提速手段。审查按当次实际资产和授权范围记录，缺少耗时证据则保持未取证，不补建旧版本。 |
| 0.9.3 Windows server 包装失败后保留 956.0 秒 build，仅续 package 103.5 秒、fixture helper 107.8 秒和 accept 20.3 秒 | 保留这个已奏效的续跑方式，不新增执行器。私有证据：`.rust-validation/release-093/windows-server-final-delivery.json`。 |
| 0.8.6 Docker wrapper 修正 SHA256 大小写比较后，复用同一 arm64 归档；该次聊天还记录了大文件跨主机上传长时间等待 | 先区分报告/传输失败和制品失败；保持归档字节，选用直接上传路径。私有恢复证据：`.rust-validation/release-086/docker-recovery/delivery-summary.json`。 |
| 0.8.7 beta 首次 root 读取超时，身份和配置确认后通过只读阶段续验；后续验收驱动修改未重建七份 payload | 保留失败历史与原身份，仅补受影响门禁，不为工具/文档修正自动重启服务或重建整版。见 [原始记录](Rust_Release_0.8.7.md#beta-两端验收与阶段续跑)。 |
| 0.9.3 明确授权的两种客户端替换保留其余五份 server/Docker 归档与 tag | 已有正确范围控制，写入 release skill；它是授权例外，不能推广为默认同版本覆盖。见 [发布记录](Rust_Release_0.9.3.md)。 |

本次验证为脚本合成测试、计划预览和文档检查，不声明端到端 release 提速实测。另发现 Unix 服务端验收会隐式 `cargo run` 构建 debug fixture helper；后续应统一 helper 与正式 release 的目标和编译参数后，在原生 Unix 主机验证，本轮保留现有行为。

## 发布、缓存和工具验证

管线不创建标签、提 PR 或自动发布 Release。发布仍使用已授权的 `release-create.sh` 入口；发布后可选用 `release-assets.json` 合并元数据/集合/摘要检查。该只读计划核对所提供的本地归档，不替代首次从 GitHub 下载后的内容复核。发布后固定安装与正式产物身份同步仍按 AGENTS.md 执行，不把本地构建的相同版本号当作同一发布二进制。

实际交付建议加 `-CleanBuildArtifacts`，在所有交付阶段成功后追加 debug 缓存维护，默认预算 40 GiB，可用 `-BuildCacheLimitGiB` 调整。管线先释放构建租约，再运行独占清理；交付失败不触发清理，清理失败保留已通过的交付收据供续跑。不要在 ChecksFile 中再清理一遍。SDK、正式包和固定安装均保留，详见[编译缓存维护](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Build_Cache.md)。

多份计划共享同一受管缓存时，等它们的构建与交付全部结束，再集中维护一次；不要给每条并行计划都追加清理。只复用原有结果、未构建/测试 Rust 的文档或流程任务不触发维护扫描。

修改执行器时运行 `pwsh tools/test-release-pipeline.ps1`，只使用自有 Git/文件夹/伪命令验证续跑、输出边界、参数、环境与产物失效，不编译应用或访问真实库。安装器使用 `tools/test-local-release-install.ps1` 验证拒绝不完整包；提供 `-RuntimePayload <已验收包>` 时在自有临时 checkout 中验证安装、重复执行、启动与数据保留，不更新工作区的真实安装。计划路由变化还需预览对应 `Plan`；本机脚本语法通过不代表 Mac、Linux 或硬件验收通过。

只调整计划参数路由时，可用 `test-release-pipeline.ps1 -Scope Profiles` 定向验证实际 Windows 计划的两种 ASIO 选择和构建环境失效；该模式使用伪工具，不执行 Cargo。执行器的缓存/失败处理变化仍使用完整回归。

许可证前置入口用 `pwsh tools/test-release-license-preflight.ps1` 检查实际解析器在隔离元数据下的各产品/目标路由及缺件拒绝；配合 Profiles 回归检查每次续跑重新预检、失败阻断，以及材料恢复后成功构建/包/验收/安装收据的复用。这些合成测试不编译应用或访问真实库。

2026-11-09 本轮上述两项合成回归、三份改动 PowerShell 的语法检查、四份原生计划预览、文档链接与 diff 检查均通过；专用 release skill 的格式验证与四场景独立演练通过。未执行应用构建/打包、原生 Mac/Linux 验收、发布、部署、安装或缓存维护，现有应用与工作数据保持原状。
