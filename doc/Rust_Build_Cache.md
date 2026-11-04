# 编译缓存维护

开发、测试和发布使用的 Cargo `target` 是可重建缓存。完成一轮实际编译/测试后，先按预算维护；发布交付须先完成打包、验收和固定安装，再清理构建缓存。纯文档改动或复用未变化结果时，不需要重复扫描缓存。

```powershell
# 默认只预览：40 GiB 预算，选择 debug，保留 release。
pwsh tools/clean-build-artifacts.ps1
# 超过预算才清理，已在预算内时不删除。
pwsh tools/clean-build-artifacts.ps1 -Action Run

# 开发检查成功后自动执行同一维护步骤。
pwsh tools/check-local.ps1 -Package audiolib-db -CleanBuildArtifacts
# 发布管线完成交付后维护；不要再在其 ChecksFile 内嵌一次清理。
pwsh tools/release-pipeline.ps1 -Action Run -Tag v0.8.7 -CleanBuildArtifacts
```

统一入口的 `-BuildCacheLimitGiB` 可调整预算，默认 40。维护失败时保留已通过的检查/交付证据，日志说明清理未完成；重试先复用仍有效的结果。`check-local.ps1` 的维护开关要求使用 PowerShell 7.3+，不带该开关的既有 PowerShell 5.1 用法保持兼容。

| 模式 | 删除范围 | 保留内容 |
| --- | --- | --- |
| `Debug`（默认） | 宿主机和 target triple 下的完整 `debug` profile | release 二进制、包、固定安装和正式验证记录 |
| `Incremental` | profile 下的 `incremental` 目录 | 已编译依赖、测试程序与发布程序 |
| `All`（显式） | debug/release、Cargo doc/tmp | target 根、Cargo/归属标记、源码、SDK、包、安装和数据 |

`-MaxGiB 0` 表示清理所选模式的全部合格目录；不是要求删除整个 target 根。`-OlderThanDays N` 按目录树最后修改时间筛选，不是最后使用时间。预算内的缓存保留；若仅保留的 release/doc 等已超过预算，结果会明确显示 `budget_met=false`，不会自行扩大删除范围。

```powershell
# 仅去掉增量编译缓存。
pwsh tools/clean-build-artifacts.ps1 -Action Run -Mode Incremental -MaxGiB 0
# 完成交付后需要彻底回收编译空间时，显式删除所有编译 profile。
pwsh tools/clean-build-artifacts.ps1 -Action Run -Mode All -MaxGiB 0
# 仅处理超过 14 天没有修改的 debug profile。
pwsh tools/clean-build-artifacts.ps1 -Action Run -OlderThanDays 14
```

每次输出一条 JSON：目标绝对路径、候选目录、删除/保留原因和前后逻辑字节估算。只遍历编译目录元数据，不哈希整套源码或工作数据库。硬链接、稀疏文件、压缩和共享卷会影响物理回收量；`removed_bytes` 是成功删除目录的逻辑大小，不是精确的磁盘空闲变化。部分删除失败会返回失败，已成功删除部分如实保留在记录中，不强制解锁或终止进程。

## 外部目录与 worktree

默认读取 `$env:CARGO_TARGET_DIR`，未设置时为 `<Root>/target`。项目自身的 target 需要真实 Cargo `CACHEDIR.TAG` 和可识别布局；外部 target 还需要与当前仓库/主机匹配的归属标记。先预览，确认目录是独占编译缓存后显式登记；不能把不同项目共用的缓存声明为本项目独占。

```powershell
pwsh tools/clean-build-artifacts.ps1 -TargetDirectory C:/audiolib-cache
pwsh tools/clean-build-artifacts.ps1 -Action Register -TargetDirectory C:/audiolib-cache -AdoptExisting
pwsh tools/clean-build-artifacts.ps1 -Action Run -TargetDirectory C:/audiolib-cache
```

空目录登记不需要 `-AdoptExisting`，但不会伪造 Cargo 标记；后续 Cargo 编译自行生成标记。不能接管其他仓库/主机已经登记的目录，也不接受缺少 Cargo 标记的历史非空目录。旧 worktree 可通过 `-Root <仍存在的 AudioLib checkout>` 维护其默认 target；无 checkout 或归属不明的历史输出先保留，不猜测所有权。每个目录分别预览和维护，不递归扫描、清空整块构建盘。

## 安全与并发

清理器验证真实 AudioLib workspace、解析后的绝对边界、Cargo/归属标记和链接；X: 映射盘和其 UNC 地址按同一物理路径比较。源目录、ASIO/LLVM SDK、Cargo/Rustup home、dist、local_release、beta_release、音频与数据目录均受保护。只删除明确选中的编译子目录，保留根和标记；未知根条目、链接或无法验证的目录使操作失败。

受管开发检查、客户端/服务端构建和 release 管线持有共享构建租约；清理使用同一 Git common directory 中的独占租约，因此不会撞上这些构建或其测试。预览只持有共享租约。管线的清理阶段在交付成功、释放构建租约后才运行，交付失败不清理缓存和诊断证据。直接运行 Cargo 等未受管命令时，清理器还会检查本机编译器及目标目录中运行的程序；发现占用返回 `blocked`（退出码 2），不会杀进程。退出码 1 为验证/删除失败，0 为完成预览、登记或所选维护。不要并行启动手工未受管的 Cargo 和清理；远程主机/容器的缓存须在匹配环境中单独确认占用和归属，不把本机检查当作远端覆盖。

清掉编译产物后，依赖它们的管线收据会在下次 Run 中失效并重新构建；未变化的源码功能检查结果仍可按原身份复用。已经验收的 `dist` 包与安装不受影响，不因为清理而重跑应用验收。Mac 不可覆盖归档的规则继续有效；需要重新交付时用新的运行目录。

维护代码变更运行 `pwsh tools/test-build-artifacts.ps1`。删除、链接、占用、外部归属及管线联动均在自有临时 Git checkout 和伪 Cargo 目录中验证；不借此清理真实 SDK、数据库或音频。
