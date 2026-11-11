# AudioLib Explorer 版本规范

当前正式版为 [0.9.5](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.5)，于 2026-11-10 14:34:54 UTC 发布，以 `v0.9.4` 为基线使用 PATCH。七份正式 payload 来自最终 main/tag `7d7a60bad31d8640441b16de0d558d3a4211a0e0`，构建、本地验收、十四项线上资产与校验正文、发布后固定安装身份同步均通过。API `/api/v1/`、普通本地写 schema 8、专用服务端迁移 schema 9、快照兼容 `3..=9` 以及 base/af1 完整格式目录均沿用 0.9.4。范围为统一菜单与全局设置、紧凑搜索标签、Waveform 选区试听与原素材 WAV 切片、源版本校验、原生指针、SoundTouch 生命周期与许可证预检。准确发布状态和原始验证身份见 [0.9.5 发布记录](Rust_Release_0.9.5.md)；下文历史标签、资产和记录保持原样。

Search Metadata v2 的 0.9.0 发布记录见[0.9.0 迁移方案](Rust_Release_0.9.0.md)。
上一正式版 [0.9.4](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.4) 于 2026-11-08 发布，相对 `v0.9.3` 为 PATCH。七份正式产物来自 main `f6adb2bd777d4db78d43cbdcee626153592c1482`；十四项线上资产/SHA256 与发布后固定安装同步通过，见[发布记录](Rust_Release_0.9.4.md)。本地写目标仍为 8，显式服务端迁移目标为 9，客户端快照读取范围仍为 3..=9。0.9.4 新增 `audio_sources:1` 与 af1 完整格式，不能据 schema 数字未变声称旧客户端可访问所有新库。后续文档提交不移动标签或改变已验收二进制源码。

0.9.3 范围为 #239 的 Waveform / Play 反馈修复、借鉴旧 #185 空页处理的连续浏览修复，以及 #241 / #243 的备份与打包修正。本轮桌面回归、严格 Clippy 及七份最终包的验收均通过，原始证据复用身份见发布记录。本次不整合该旧 PR 的视觉重做、分页偏好和草稿工具等新功能。

已发布 `v0.9.3` 的两个客户端随后按用户明确要求替换，以补齐 Classic 播放、暂停、停止、循环与 Mono 的统一状态反馈。此次例外保留原 tag 提交 `5ca7e59c7fcadc22043f0a030c8d496854809b23`，只替换 Windows x64 / macOS arm64 客户端及各自校验文件；两份新客户端从 main `81ac352fdd73a009197a1442887dc65256aca302` 构建并分别验收，五份服务端归档及其原始身份和摘要不变。替换时间、新旧客户端 SHA256 与验收结果见 0.9.3 发布记录，不把新客户端冒充原 tag 产物。已安装 `0.9.3` 的用户须手动重新下载更新；同版本不会再次触发更新提示，Mac ad-hoc 签名仍非 Apple 公证。此项明确授权不扩展为常规同版本资产替换流程。

0.9.4 范围为新音频源 MP3、AIFF/BWF、RIFF/RF64/BW64 边界与解码修正、原始非 WAV 远程试听、搜索任务标签自适应与关键词优先，以及隐藏缓存所有权标记的工具修正。上述 0.9.3 替换历史保持原样。

程序、API、schema 和派生 feature 各有版本，兼容性由[共享契约与完整格式目录](api/Library_Migration_Compatibility.md)共同判断，不能互相推导。0.9.5 相对 v0.9.4、0.9.4 相对 v0.9.3、0.9.3 相对 v0.9.2 均为 PATCH；0.9.2 相对 `v0.9.1`、0.9.1 相对 `v0.9.0` 均为 PATCH；0.9.0 相对 `v0.8.7` 已完成 MINOR 升版。历史记录保留原来的身份。

| 版本 | 来源（唯一事实） | v0.9.3 基线 | 0.9.4 | 0.9.5 | 含义 |
|---|---|---|---|---|---|
| 程序版本 | 工作区 `Cargo.toml` 的 `[workspace.package] version` | 0.9.3 | 0.9.4 | 0.9.5 | 客户端、CLI、服务端共用，发布时一起升 |
| API 版本 | URL 路径前缀 `/api/v1/` | v1 | v1 | v1 | 不兼容改动才换成 `/api/v2/` |
| 普通本地写目标 | DB `SCHEMA_VERSION` 与 Core `compatibility::TARGET_SCHEMA` | 8 | 8 | 8 | 常规本地写者仍拒绝s9写入 |
| 专用服务端写目标 | Core `compatibility::SERVER_TARGET_SCHEMA`，由 `LibraryFormat::server_schema9()` 使用 | 9 | 9 | 9 | 显式服务端迁移及专用s9写者，不从本地常量推导 |
| 客户端快照读取范围 | DB `MIN_SNAPSHOT_SCHEMA_VERSION` 至 Core `compatibility::SNAPSHOT_SCHEMA_MAX` | 3..=9 | 3..=9 | 3..=9 | 还须校验完整格式、feature、结构与身份 |

普通本地 base 格式仍为 `ale-s8-cld1-t3-m2-p1-a1`，服务端 schema-9 base 格式为 `ale-s9-cld1-t3-m2-p1-a1-sd1`。0.9.4 同时支持必需 feature `audio_sources:1` 对应的 `ale-s8-cld1-t3-m2-p1-a1-af1` / `ale-s9-cld1-t3-m2-p1-a1-sd1-af1`；该 feature 只在首次成功发布扩展源记录的同一事务中启用，原格式库和空库保持原 ID。必需 feature、目录结构和身份按实际完整格式校验，不从程序 PATCH 或 schema 数字推导。

旧客户端不支持 af1；既有 schema-9 WAV-only 投影继续排除新增容器、恢复历史 schema-8 投影身份，不把旧客户端升级为新源能力。导入、迁移和回滚保留 af1 要求；启用共享本地库的新源前关闭旧版读写实例，回退旧程序须使用匹配的兼容备份或受支持的只读投影，不能删除 feature 标记冒充兼容。Metadata 仍为原始 JSON TEXT；压缩实验格式 `ale-s8-cld1-t3-m2-p1-a1-z1` 的 `metadata_storage=1` 仅保留为严格只读兼容来源，不在可写目录或迁移目标中。快照外层 zstd 传输与 Metadata 列存储分别协商。

快照读取边界为 `MIN_SNAPSHOT_SCHEMA_VERSION..=SNAPSHOT_SCHEMA_MAX`：已发布0.8.7为 `3..=8`，当前 0.9.5 为 `3..=9`。普通本地写目标8不是读取上限，`MAX_KNOWN_SCHEMA` 也不是服务端迁移目标。客户端先验证快照身份、结构、完整性、storage、tokenization、Metadata、feature及迁移协议；完整s8/s9快照按各自契约只读打开。schema3–7的派生升级和共享专辑投影仅在客户端拥有的 staging/copy 中运行，原始只读文件保持不变。收紧读取下限属于不兼容变化，应评估 MAJOR 或 API 版本。

## 规则

1. **本地或专用服务端可写目标改变，程序版本至少升 MINOR**：分别比较 `SCHEMA_VERSION` 与 `SERVER_TARGET_SCHEMA`；旧标签没有服务端独立常量时，基线服务端目标沿用其本地目标。单独拓宽快照读取上限属于兼容能力增加，按功能及兼容策略决定版本，不能冒充写目标变化。历史0.8.1使用下述明确基线例外。
2. 标签必须是 `vX.Y.Z`，预发布为 `vX.Y.Z-rc.N` 或 `vX.Y.Z-beta.N`，且 `X.Y.Z` 必须与 `Cargo.toml` 一致。其他预发布后缀会被拒绝。
3. 程序版本不得低于基准标签的版本。
4. 服务端与远程客户端源码中出现的 `/api/vN` 必须只有一个版本。
5. `doc/Rust_Release_Notes.md` 的现代候选必须有**一行**同时写明程序、API、本地写目标、服务端写目标和实际快照读取范围；旧标签保留原来的单schema版本行。0.9.5 版本行示例为：

   `程序 0.9.5 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9`

6. 发布产物附 SHA256：各平台打包器生成 `<archive>.sha256`，本机发布步骤把它连同包上传为 Release 资产，并写入发布说明正文。
   从 0.8.4 起，每个 Release 精确发布六个归档及各自的 `.sha256`，共十二个资产：Windows x64 客户端、Mac arm64 客户端、Mac arm64 服务端、Linux x64 服务端，以及 Docker linux/amd64、linux/arm64 离线镜像。文件名分别为 `AudioLibExplorer-<tag>-win-x64-portable.zip`、`AudioLibExplorer-<tag>-macos-arm64.zip`、`AudioLibServer-<tag>-macos-arm64.tar.gz`、`AudioLibServer-<tag>-linux-x64.tar.gz`、`AudioLibServer-<tag>-docker-linux-amd64.tar.gz`、`AudioLibServer-<tag>-docker-linux-arm64.tar.gz`。0.8.3 保留三个包、六个资产；0.8.2 及更早版本保持两个包、四个资产。Mac 客户端手动更新，Docker 导入镜像并重建容器；Windows 客户端及 Mac arm64、Linux x64 原生服务端保留程序内更新。从 0.8.5（首个纳入版本）起增加 Windows x64 服务端 `AudioLibServer-<tag>-win-x64.zip`，共七个归档、十四个资产（门槛常量见 `tools/verify-release-assets.ps1` 的 `$WindowsServerFirstVersion`）；0.8.4 及更早版本保持原资产集合，源码构建仍可用。详见 [Windows 服务端发布](Rust_Windows_Server_Release.md)。独立 Linux arm64 服务端包不发布。

规则1到5由 `tools/check-release.ps1` 检查，失败时列出错误并以非零退出。`tools/test-check-release.ps1` 在隔离合成Git资料库验证服务端单独升版、历史基线、读取上限及版本行，不创建本仓库标签或运行应用。0.9.0相对v0.8.7已升MINOR，满足服务端可写目标8→9的升版要求；0.9.4 相对 v0.9.3 为 PATCH，写目标未变，但新增 af1 完整格式与旧客户端边界须单独说明。版本预检不替代 feature 兼容检查、源码、实际包和平台验收。

## 发布流程

本机阶段优先使用[Release 管线](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Release_Pipeline.md)：`release-pipeline.ps1` 默认预览，`-Action Run` 串联已有构建/打包/验收，Windows 客户端还包含固定安装。一次调用只输出短状态，完整日志和续跑收据留在 `.rust-validation/`；不要逐阶段重新拼命令或反复读取长报告。源码测试仍按实际影响选择，管线不自动扩大全量矩阵，也不创建标签或自动发布。

不得覆盖既有标签或把新构建当作旧 Release 的原产物。[v0.8.7](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.8.7) 已正式发布，以 `v0.8.6` 为发布基线；API、schema 和完整存储格式未变，因此使用 PATCH 版本。正式标签与七份产物源码固定为 `b5a98ff98fed501aafff0f6a4ef5015c8b4d435d`，后续 main `d0168f74182f52f493adb2a3b1fc9af0f3ec4eac` 仅整合验收工具与流程记录，不移动标签或重建已验收资产。既有 `v0.8.6` 及更早标签和资产保持原发布身份。本轮范围、各阶段验收身份、远端发布核验及覆盖限制见 [0.8.7 发布记录](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Release_0.8.7.md)。以下流程供后续发布沿用。

历史基线说明：`v0.8.0` 的 Release 已撤回，但标签保留。0.8.1 当时按用户确认使用最后实际公开的 `v0.7.8` 为发布基线：程序 MINOR 从 7 升至 8，可写 schema 6 库升级为 8；撤回的 0.8.0 开发库（schema 7）也通过受保护迁移升级为 8。回退旧程序须配套恢复相应旧 schema 备份。这个例外不重写既有标签，也不改变后续版本的版本升级规则。

1. 把 `Cargo.toml` 的 workspace version 改为目标版本（同时更新 `Cargo.lock`）。
2. 在 `doc/Rust_Release_Notes.md` 写发布说明，并加入上述版本行。
3. 本地预检（不创建标签，标签用参数传入）；0.9.4 的 Cargo/锁文件更新后使用：
   `pwsh tools/check-release.ps1 -Tag v0.9.4 -BaseTag v0.9.3`
   该检查只验证版本契约；正式发布仍须完成下述适用验收。
   仅标签去除 beta/rc 后缀后为 `v0.8.1`且未给出 `-BaseTag` 时，脚本默认 `v0.7.8`；显式 `-BaseTag` 始终优先。其他版本仍默认取除自身外最新的 `vX.Y.Z` 标签。显式以撤回标签 `v0.8.0` 作基线仍会按 schema 7→8 检查并拒绝仅 PATCH 的比较；这不是移动或替换该标签。
4. 正式发布前按实际改动选择 package、直接消费者及专项回归，使用实际发布的 target/features 检查一次。`check-local.ps1 -Package <packages>` 支持按包执行；只有影响广泛或无法判断时才使用全 workspace。客户端、无播放服务端及各平台不因同属一次 Release 而互相重复功能测试；无 ASIO 条件分支仅在受影响时定向补测；相关应用/测试源码、feature、工具链与环境未变时复用已通过结果，标签或文档变化本身不触发 Cargo 重跑。日常按 [Testing Strategy](Testing_Strategy.md) 选择最小范围；修复后只重跑失败和受影响检查，不删减数据安全回归。缺少平台或硬件覆盖时明确记录。
5. beta 门槛通过并合并后，在已获发布授权的发布提交上打标签 `v0.9.4`（或对应的预发布标签）并推送。
6. 每份正式 payload 只在对应本机平台从最终 main/tag 提交构建、打包并完整验收一次；beta 预编译产物不能替代最终提交身份。Windows 客户端用 `tools/build-release.ps1` 仅构建 desktop/CLI binaries，再用 `tools/package-portable.ps1 -SkipBuild -Tag <tag>` 打包。也可用兼容手动入口 `tools/ci-release.ps1 -Tag <tag>` 替代这两步，不串行重复；该入口的运行时报告为 `not-run`，随后仍须运行一次 `pwsh tools/validate-portable.ps1 -BinaryDirectory dist/latest/portable`（默认隔离合成数据；仅旧库导入路径变化时显式用 `-LegacyImport`）。Windows 的 ASIO 构建、打包与许可证检查使用同一 feature 选择。Mac 客户端使用 `tools/build-macos-client.ps1 -ExpectedCommit <full-commit> -ReportPath <private-build-proof.json>`，再在已登录桌面的 Mac 上运行 `tools/package-macos-client.ps1 -Tag <tag> -ExpectedCommit <full-commit> -BuildReport <private-build-proof.json> -ValidateRuntime`，结构与原生验收一次完成。仅延后运行验收、产物变化或换机复核时单独运行 `validate-macos-client.ps1`；完整原生验收仍须通过，`-StructureOnly` 的 `not-run` 结果不能替代它。首次 Mac 客户端分发计划使用 ad-hoc 签名、未经 Apple 公证；签名与启动以实际 Mac 报告为准。独立 server 使用 `build-server-release.ps1`、`package-server.ps1` 和 `validate-server-package.ps1`；Windows 服务包验收需用 benchmark 创建自有 fixture 时，显式构建 `cargo build -p audiolib-db --bin search-benchmark --release --locked`，普通客户端/服务端构建不顺带生成它。各 Unix 包在匹配架构环境中构建，Unix 不传 `-Asio`；Linux arm64 包仅作为 Docker 输入。Docker 镜像从已验收的公开 Linux 包生成，分别构建单平台镜像并保存离线归档，禁止把工作目录或运行数据作为正式镜像上下文。详见 [Linux 与 Docker 交付](Rust_Linux_Docker_Release.md)。实际包的文件白名单、隐私、身份、许可证和启动/health 均在本地验证。
7. 按上述版本规则上传精确资产集合：用 `tools/release-create.sh <tag> <资产…>` 创建 Release 并上传，或对已创建的 Release 用 `gh release upload <tag> <资产…>`。之后可选地在 GitHub Actions 中手动触发 `.github/workflows/release.yml`（仅 `workflow_dispatch`，输入 `tag`）做只读核验。共享 `tools/verify-release-assets.ps1` 校验标签、标题、prerelease 标志、精确资产集合和 SHA256；0.8.5 起十四项、0.8.4 十二项、0.8.3 六项、更早标签四项，缺失、额外、重复或大小写不符均失败。该 workflow 不构建、不解包验收、不创建/编辑 Release、不上传资产（`contents: read`）；推送标签不会触发它，因此打标签前无需先禁用 release.yml。按 AGENTS.md 更新固定 `local_release/`，核对包内文件哈希、CLI 版本和一次隔离启动；同一已验收 payload 不再重复整套 portable。下载的已发布产物须先验证归档摘要，再使用 `-PublishedReleaseReport <report> -ExpectedTag <tag> -ExpectedCommit <full-commit> -CleanCheckout` 模式验收；`ExpectedTag` 和 `ExpectedCommit` 不能脱离发布报告单独使用。发布后按 AGENTS.md 的 Published release synchronization 核对本地安装与正式资产身份，并保留原数据目录、资料库和偏好设置。

   本地可将 `gh release view <tag> --json tagName,name,isPrerelease,assets` 的 JSON 保存为文件，先运行 `pwsh tools/verify-release-assets.ps1 -Tag <tag> -ReleaseJsonPath <metadata.json>` 获得预期包名列表；下载后增加 `-AssetDirectory <downloaded-directory>` 核对全部摘要。`tools/test-release-assets.ps1` 使用隔离合成文件验证这套规则，不连接 GitHub。

常用模拟（`-Root` 可指向临时克隆，便于不改工作区验证）：

- 通过：Cargo 0.4.6 + 版本行 + `-Tag v0.4.6 -BaseTag v0.4.5`
- 失败：`-Tag v0.4.12`（与 Cargo 不一致）；schema 变了只升 PATCH；缺版本行。

## Release 命名规则

- 正式版：标签 `vX.Y.Z`，Release 标题 `AudioLib Explorer vX.Y.Z`，不是 prerelease。
- Beta：标签 `vX.Y.Z-beta.N`（rc 同理 `-rc.N`），标题 `AudioLib Explorer vX.Y.Z-beta.N`，必须标为 prerelease（`--latest=false`）。
- 标题、标签与 `Cargo.toml` 工作区版本（忽略 `-beta.N` / `-rc.N` 后缀）必须一致。标题由标签自动生成：`tools/release-create.sh <tag> [--notes-file f] [资产…]` 传 `--title "AudioLib Explorer <tag>"`，不要手写标题。
- 发布后用 `pwsh tools/check-release.ps1 -Tag <tag> -CheckGitHubRelease` 核对（或 `-ReleaseTitle <title> -Prerelease <bool>` 离线检查）；标题不符或 prerelease 标记与标签后缀不符均为硬错误。

## Beta 流程

1. 功能 PR 合入 `beta` 分支；`main` 只接受经过 beta 测试的 `beta`（或 hotfix 分支）合并。
2. 在 `beta` 的待测提交上打 `vX.Y.Z-beta.N`（`X.Y.Z` 与 `Cargo.toml` 一致），发布为 GitHub pre-release（`tools/release-create.sh <tag>`，等价于 `gh release create <tag> --title "AudioLib Explorer <tag>" --prerelease --latest=false`；完全由标签推导：带后缀的标签（`-beta.N` / `-rc.N`）自动加 `--prerelease --latest=false`，纯 `vX.Y.Z` 为正式版）。
3. 本机用 `tools/install-beta.ps1` 安装到独立目录（默认仓库下 `beta_release/`，可用 `-Destination` 指定），并通过 `AudioLibExplorer-beta.cmd` 启动；该脚本把 `AUDIOLIB_DATA_DIR` 设为 `%LOCALAPPDATA%/AudioLibExplorer/beta`，与正式版 `v1` 数据完全隔离。
4. 有应用改动的 beta 候选按 [AGENTS.md](../AGENTS.md) 的 Beta testing flow 执行自动验收：Mac 上 `tools/beta-refresh.sh` 刷新 beta 服务端（含 commit 与库根读取自检），Windows 上 `tools/beta-smoke.ps1` 冒烟。GUI 验收按本版实际 UI 改动和风险选择相关交互或视觉检查，仅在具体风险需要人工观察时请求确认，不默认重复历史固定清单。纯文档或流程规则更新按 Development checks 的范围复用仍有效的通过证据，不重建未变客户端、不重复无关验收；保留原证据的源码提交和二进制身份。必要检查的真实失败仍阻止提升。
5. beta 验收通过后，从 `beta` 开 PR 合入 `main`，再按上文流程打正式标签。
