# 0.9.5：统一菜单与 Waveform 选区试听、切片

程序 0.9.5 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已于 2026-11-10 14:34:54 UTC（北京时间 22:34:54）正式发布 [v0.9.5](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.5)，以 `v0.9.4` 为基线使用 PATCH。经 [#267](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/267)、[#268](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/268) 完成 beta 验收和 main 提升；七份正式 payload 来自 main `7d7a60bad31d8640441b16de0d558d3a4211a0e0`。构建、本地验收、十四项线上资产与校验正文、发布后固定安装身份同步均通过，详见 [0.9.5 发布记录](Rust_Release_0.9.5.md)。API、写目标、快照读取范围及完整格式目录沿用 0.9.4，包括已有 `audio_sources:1` / af1 边界。

- **菜单与设置**：三个主题使用统一的平台菜单栏，提供 View / 视图下的主题、语言和侧栏入口。全局设置按用途分组，音频与同步草稿分别管理；保留已有偏好与播放上下文。搜索工具栏和来源状态提示在窄窗口中保持可用。
- **紧凑搜索标签**：关键词和目录使用同一行的紧凑文本，关键词优先，目录仅在不挤压关键词时出现；标签文本靠左，完整条件仍可悬停查看。
- **Waveform 选区试听**：经典与现代播放器支持正向或反向框选、端点调整、Shift 重新框选与 Esc 取消。选区绑定当前显示的声音，按原素材采样帧保存；松开后试听该范围，Stop、播放结束后重播和循环遵循选区边界。
- **原素材 WAV 切片**：拖出选区时后台准备 WAV，保留原采样率和全部源声道，忽略试听的 Pitch、Rate、Mono 与音量。原生目标接受复制后保留独立的编号文件，不覆盖已有输出；未交付的文件按拥有边界清理。输出仅重建必要音频结构，不复制源文件的描述、封面、cue、BEXT、iXML 或未知厂商块；源音频与资料库 Metadata、用户标注不变。
- **来源与反馈**：远程切片使用与显示版本一致的原始音频，绑定强 ETag、长度和版本凭据；原件不可验证或已变化时拒绝切片，不使用低质量预览代替。Waveform 增加匹配操作状态的原生光标与准备进度，光标大小随系统 DPI，保持独立于应用缩放。
- **音频与诊断**：配对销毁原生 SoundTouch 处理器，修复 Windows 包装层遗漏析构导致的缓冲积累；原生版本与试听算法保持不变。同步 Rust panic 记录省略 panic payload、配置和运行参数，原生崩溃仍需对应系统证据。
- **发布工具**：在构建前预检实际目标及 feature 所选依赖的精确许可证材料，缺件时阻止继续构建；打包与运行验收保留各自的许可证检查。

七个正式归档及其 SHA256 共十四项线上资产已核验；内部 Linux arm64 原生包仅供 Docker 输入。Mac 已新增严格 Clippy、180 项音频与 93 项 Waveform 测试，并通过最终包的原生运行及 Quit；Windows 固定安装 verified=907、copied=23，CLI 0.9.5 与隔离启动通过，发布后全文件身份再次复核。Windows server 真实服务优雅关闭、Explorer/Finder/DAW 跨程序拖出、真实 ASIO 硬件和声学起音延迟未测；Mac ad-hoc 签名不是 Apple 公证，Linux x64/Docker amd64 验收使用 ARM64 主机仿真。

# 0.9.4：新音频源格式与自适应搜索任务标签

程序 0.9.4 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已于 2026-11-08 15:28:34 UTC 正式发布 [v0.9.4](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.4)，以 `v0.9.3` 为基线。功能来自 [#255](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/255)、[#256](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/256)，经 [#257](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/257)、[#258](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/258) 提升至 main `f6adb2bd777d4db78d43cbdcee626153592c1482`。七份正式 payload、十四项资产、线上 digest/校验正文和发布后固定安装同步通过，详见 [0.9.4 发布记录](Rust_Release_0.9.4.md)。后续文档提交不改变二进制源码或标签身份。

- **扫描与 Metadata**：默认扫描新增 `.mp3`、`.aif`、`.aiff`、`.bwf`；根据实际容器字节识别格式。MP3 提取有界 ID3 文本、重复及未知字段、可选封面和 ID3v1 fallback；AIFF 提取 NAME/AUTH/ANNO/COMT/ID3，并支持明确的 AIFF/AIFC PCM/float 子集。用户标签、备注、收藏等标注保持独立。
- **RIFF 边界**：子块遍历限制在 RIFF 或 RF64/BW64 的父容器声明范围内，容器外私有尾信息不再使合法音频误报截断；真正的音频块越界、内部结构异常及错误 PCM 布局仍拒绝。
- **原文件试听**：非 WAV 远程试听使用原始 `/stream` 字节。MP3 采用实际完整帧计数与已验证的 gapless trim；解码器内存中的可信信息帧避免坏 Xing/Info 字段覆盖准确时长，不改写源文件、HTTP 内容或下载缓存。AIFF 使用明确的字节序、signed PCM 和 float 布局，seek 与 Waveform 共用解码路径。本版不承诺新的实际播放启动延迟或主观音质测量结果。
- **兼容边界**：首次成功写入新增音频源时，同一事务启用必需 feature `audio_sources:1`。完整格式新增 `ale-s8-cld1-t3-m2-p1-a1-af1` / `ale-s9-cld1-t3-m2-p1-a1-sd1-af1`；未启用扩展源的库保持原格式，schema 数字不变。旧客户端不能凭 schema 8/9 放行 af1；既有 schema-9 WAV-only 兼容投影继续排除新增容器。共享本地库启用新源前须关闭旧版读写实例。
- **搜索任务标签**：标签先均分可用宽度，最窄为 100% UI scale 下 144 逻辑像素，再进入横向滚动；滚动条保留独立底部空间。关键词优先显示，前缀和目录只在不挤压关键词时出现。
- **构建缓存工具**：修正隐藏所有权标记的读取，并加入真实 Hidden 标记的预览、清理及 release 输出保留回归；不扩大清理权限或外部缓存的认领范围。

升级前保留受支持的资料库备份；先升级需要访问新源的客户端和服务端，再进行扫描。启用 af1 后，回退旧程序须使用兼容的备份或受支持的只读投影，不能手工移除 feature 标记。旧 `v0.9.3` 的两份客户端替换身份继续按原发布记录保留，不在本版重新计算为新功能。

# 0.9.3：Waveform、播放反馈与连续浏览修复

程序 0.9.3 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已于 2026-11-07 发布 [v0.9.3](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.3)，以 `v0.9.2` 为基线。完整格式保持 `ale-s8-cld1-t3-m2-p1-a1` / 服务端 `ale-s9-cld1-t3-m2-p1-a1-sd1`，不引入数据库迁移。正式资产、SHA256 及验收边界见 [0.9.3 发布记录](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/beta/doc/Rust_Release_0.9.3.md)。

两个客户端于 2026-11-07 20:44:57（北京时间）按用户明确授权替换：Windows x64 / macOS arm64 客户端来自 main `81ac352fdd73a009197a1442887dc65256aca302`，原 tag `v0.9.3` 仍指向 `5ca7e59c7fcadc22043f0a030c8d496854809b23`。其余五份归档与各自校验文件（共十项资产）保持原始发布字节。已安装 `0.9.3` 的用户需手动重新下载客户端更新，同版本不会再次弹出更新提示；Mac 包继续为 ad-hoc 签名，未经 Apple 公证。新旧客户端 SHA256 和各平台实际验收结果见发布记录。

- **Waveform（#239）**：修复已播放区域在不同 DPI 和缩放位置出现细接缝的问题，保持连续的播放进度着色。
- **播放反馈（#239；客户端替换 #246 / #247）**：Classic 播放、暂停、停止、循环和 Mono 统一移除激活下划线，使用边框与底色表达状态；保留键盘、启用条件和无障碍选中状态。
- **连续浏览（借鉴 #185 的空页处理）**：服务器移除记录后，连续浏览会越过被隐藏的整页结果，保留前后导航、任务切换与恢复位置。导航失败时保留当前结果；后台资料库变化时重新读取，避免续查遗漏或重复。
- **备份与打包（#241 / #243）**：仅对新建的自有 WAL 备份完成独立镜像收尾，源库保持只读；补齐 Mac 实际依赖的精确版本许可证材料及校验出处。

本次不改变源 WAV、Metadata 存储格式、HTTP API 或服务端写目标。发布范围包括 Windows x64 / Mac arm64 客户端，Windows x64 / Mac arm64 / Linux x64 服务端，以及 Docker linux/amd64 / linux/arm64，共七个归档及各自 SHA256。执行进度与平台覆盖见上述发布记录。

# 0.9.2：服务器模式重启后保留封面，并避免对当前客户端误弹升级窗

程序 0.9.2 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已发布。以已发布的 `v0.9.1` 为基线。完整格式保持 `ale-s8-cld1-t3-m2-p1-a1` / 服务端 `ale-s9-cld1-t3-m2-p1-a1-sd1`，不引入数据库迁移。

- **服务器模式封面与启动同步（#236）**：打开上次缓存快照后就会请求 `/capabilities`，不再只在点同步成功后才拉取。在 capabilities 尚未就绪时继续使用已经读到的磁盘封面，避免重启后封面空白、必须再同步一次。
- **升级提示**：GET `/capabilities` 带上兼容头；当正在运行的客户端已经达到 `min_client_version` 时，不再弹出 Software update 窗口。

本次不改变源 WAV、Metadata 存储格式、HTTP API 或服务端写目标。

# 0.9.1：sidecar-v1 跨子目录与 Docs/Sounds 兄弟目录匹配

程序 0.9.1 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已发布。以已发布的 `v0.9.0` 为基线。完整格式保持 `ale-s8-cld1-t3-m2-p1-a1` / 服务端 `ale-s9-cld1-t3-m2-p1-a1-sd1`，不引入数据库迁移。

- **扫描 sidecar-v1 表格匹配（#233）**：自动发现的 CSV/TSV/XLS/XLSX 在匹配 WAV 时，可覆盖表格目录子树，以及扫描根下同 stem 的 `{stem}--Docs` / `{stem}--Sounds*` 兄弟目录。
- 仅当表格所在目录本身是 `{stem}--Docs` 或 `{stem}--Sounds*` 时，才扩展到同 stem 兄弟；并排不同 pack 不会交叉匹配。
- 身份与文件名校验不变；范围内重名 basename 仍视为歧义；不会按 basename 搜索资料库根。

本次不改变源 WAV、Metadata 存储格式、HTTP API 或服务端写目标。

# 0.9.0：扩展搜索与 Metadata 来源

程序 0.9.0 | API /api/v1/ | 本地写 schema 8 | 服务端迁移 schema 9 | 快照兼容 3..=9

已发布。进度和证据见 [发布准备](Rust_Release_0.9.0.md)。下文已发布 0.8.7 的历史格式与身份保持原样。

- **路径可搜（#204）**：新增目录段与祖先目录搜索，支持中文分词；提供主字段、
  仅文件名、仅路径、全部四种范围。全部支持主字段与路径之间的 AND 组合；
  仅路径中的每个词都必须命中目录。范围随搜索任务保存，筛选、排序和翻页保持一致。
- **Vorbis 标签（#205）**：扫描 FLAC、Ogg Vorbis/FLAC 与 OGA，保留重复、中文及未知文本标签，
  接通 Metadata 显示和搜索。存量文件可重新提取，保留用户标注；图片不进入 FTS5。
  随包支持 FLAC/Ogg Vorbis 解码试听，Opus 仅支持标签探测。
- **厂商表格（#206）**：导入 CSV/TSV、BIFF XLS 与 XLSX，支持编码/BOM、表头、多个 sheet、
  列映射和模板。先预览，再按相对路径或文件名匹配；歧义候选可分页并明确确认。
  提交提供批次、进度、取消/续传、历史和回滚；保留原始来源及用户标注，厂商文本可搜索。
  本地和有管理权限的服务端导入使用同一业务契约。
  扫描还会自动发现同目录CSV/TSV/XLS/XLSX，仅关联本轮成功扫描的唯一WAV；先完整解析，
  再分批补充Metadata，按嵌入及手动来源去重并保留独立用户标注。完整关联的表格变更会更新自身贡献；
  存在未匹配或歧义时保留该来源全部旧贡献并加入安全新值，修正表格或显式回滚后可收敛。
  损坏、空表或暂时缺失不会自动删除最后有效Metadata，增量重扫支持仅表格变化及新WAV。
- **客户端与服务端迁移**：普通本地库继续写 schema8，目录索引准备不冒充 schema9 转换。
  服务端由显式迁移发布 schema9，提供备份、进度、取消/恢复和回滚；新客户端读取校验过的
  schema8/9 快照，支持 Metadata 增量与离线重开。过渡期默认保留旧客户端的 WAV-only schema8 派生快照。
- **配套界面**：现代主题提供迁移管理、厂商导入和刷新入口；升级提示按库身份去重。
  保留 Default v2 与播放器反馈改进。

迁移顺序为先更新可读 schema8/9 的客户端，再更新处于 deferred 状态的服务端，最后由管理员
显式启动迁移。首个包含独立迁移工具的服务端包须手动引导安装，旧更新器会拒绝新布局。
数据库回退必须配合匹配的备份及新写入保护；详情见发布准备中的操作和恢复方案。

# 0.8.7：30 个搜索任务、主题切换与播放重绘

程序 0.8.7 | API /api/v1/ | schema 8 | 快照兼容 3..=8

以已发布的 `v0.8.6` 为基线，完整格式保持 `ale-s8-cld1-t3-m2-p1-a1`，不引入数据库迁移。

- 搜索任务最多可同时打开 30 个，新安装默认上限为 30；保留已保存的个人上限（包括 6），可在外观设置中调整。达到上限时不替换已有任务。
- 两种主题的任务栏在溢出时提供左右翻页箭头，新建和激活任务自动显示对应标签；新建 `+` 固定在滚动区旁，任务较多时仍可直接操作。
- 个人工作区保留任务顺序、当前任务与浏览状态；工作区格式版本仍为 2。旧客户端无法读取超过 6 个任务的工作区时保留原文件并只读回退，防止降级覆盖丢失任务。
- 恢复 Default 默认播放器，并新增可在外观设置中切换的 Studio 工作台；保留现有试听、Waveform、界面缩放、音量和音高/速率状态。
- 修复主题切换时的输入反馈、焦点和 Waveform 来源归属；切换时保留选择、滚动位置及播放上下文。
- 区分搜索任务与主题工作台的视图恢复，修复结果追加或远程/本地模式切换后的列表滚动回退；保留小数行位置及目录、筛选和详情面板的独立位置，并避免过期来源覆盖当前视图。
- Studio 使用细播放头和可见的播放/暂停按钮过渡；播放区域分块绘制，工作台背景预先栅格化，减少持续播放时的软件重绘开销。
- Windows 按原生窗口曝光情况请求完整重绘，持续播放使用局部重绘；保留恢复、调整大小与遮挡后的画面恢复路径。
- 修复 Windows Common Controls v6 加载依赖，避免桌面程序在进入主界面前启动失败。
- 本地 beta 安装使用独立开发身份，防止自动更新误跳转到已发布的旧稳定包；新增可续跑的本地发布管线与有预算的编译缓存维护。
- 原生窗口重绘验证可分别选择 Default / Studio，Studio 使用独立报告，避免后运行的主题覆盖先前结果。

本轮所选源码检查、交付、原生验收、beta 门禁及最终多平台产物验收已完成，[v0.8.7 正式 Release](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.8.7) 已发布七个归档及各自 SHA256，共十四项资产。标签与实际产物源码冻结为 `b5a98ff98fed501aafff0f6a4ef5015c8b4d435d`；后续 main `d0168f74182f52f493adb2a3b1fc9af0f3ec4eac` 仅整合验收工具和流程记录，没有移动标签或重建已验收产物。范围、历史失败及各项实测身份见 [0.8.7 发布记录](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Release_0.8.7.md)。源 WAV、Metadata、资料库格式和服务端 API 沿用现有契约。

Mac 客户端使用 ad-hoc 签名，未经 Apple 公证；Linux x64 服务端与 Docker linux/amd64 在 ARM64 主机的 AMD64 仿真环境验收，未覆盖物理 x64 硬件。没有新增声学输出延迟测量或真实 ASIO 硬件试验。

# 0.8.6：连续浏览与试听播放条

程序 0.8.6 | API /api/v1/ | schema 8 | 快照兼容 3..=8

以已发布的 `v0.8.5` 为基线，完整格式保持 `ale-s8-cld1-t3-m2-p1-a1`，不引入数据库迁移。

- 搜索结果支持连续滚动和按需加载；跨页选择、键盘导航及搜索任务恢复保留当前位置，减少分页打断。
- 完善音效库初始化和迁移进度提示，在耗时操作中显示当前阶段。
- 试听播放条明确显示加载、就绪、播放、暂停、结束、错误和缺失文件状态；加载失败可重试，缺失 WAV 可通过现有重新关联流程修复引用。
- Waveform 支持拖动预览和定位、悬停时间、整刻度时间尺及支持 Tab、方向键与 Home/End 的缩放概览；播放超出当前可见范围时翻页跟随，手动浏览后保留用户视图。
- 增加静音后恢复原音量、音量滚轮和键盘调整、无效音高或速度输入提示，以及输出设备与欠载反馈；统一控制反馈与过渡动画。

本次不改变源 WAV、Metadata、资料库格式或服务端 API。正式资产保持七个归档及各自 SHA256；各平台最终提交身份、自动验收和已知覆盖限制随实际发布记录提供。

# 0.8.5：服务端封面可靠性与 Windows 服务端

程序 0.8.5 | API /api/v1/ | schema 8 | 快照兼容 3..=8

以已发布的 `v0.8.4` 为基线，完整格式保持 `ale-s8-cld1-t3-m2-p1-a1`，不引入数据库迁移。

- 新增 Windows x64 原生服务端 ZIP 及 SHA256；与现有六个平台归档一起，正式发布要求七个归档、十四个资产。服务端通过 `--config` 或 `AUDIOLIB_SERVER_CONFIG` 读取显式配置；建议把配置、资料库与运行数据部署在 `%ProgramData%\AudioLibServer`，与程序文件分开。Windows 服务安装与管理仍属后续工作。
- 修复从服务器模式返回本地模式后结果列表无法滚动的问题；恢复搜索任务时同步校正列表滚动位置和请求状态。
- 服务器目录封面繁忙时，客户端按受控等待时间重试，保留当前内容并避免误报需要同步快照；来源切换和关闭时取消过期任务。
- 服务端对短时封面任务排队，返回一致的重试提示；拒绝被截断的封面缓存，重新生成有效内容，避免把残缺图片交给客户端。
- 统一未授权 GET/HEAD 响应的 Bearer 认证头，并修正令牌管理提示；API 路径、权限与快照格式保持原契约。
- HTTP 读取遇到系统调用中断（`EINTR`）时，在同一请求内继续读取，保留原有取消状态和超时期限，避免信号中断造成过早的网络错误。
- 修正 macOS 服务端部署的停止、等待退出、替换和启动顺序，并在启动检查中明确预期提交；保留已有 launcher 的文件访问授权和失败回滚机制。

Windows 服务端不会捆绑 ffmpeg；需要 FLAC 预览时，应在配置中填写本机 `ffmpeg.exe` 的绝对路径。Windows 客户端和原生服务端沿用各自的更新器，Mac 客户端手动更新，Docker 通过替换镜像更新。

本轮 beta、人工 GUI、各平台本地验收与正式产物身份以对应发布记录为准；历史 0.8.4 验收不替代本轮结果。

# 0.8.4：目录箭头与 Linux / Docker 服务端

程序 0.8.4 | API /api/v1/ | schema 8 | 快照兼容 3..=8

以已发布的 `v0.8.3` 为基线，完整格式保持 `ale-s8-cld1-t3-m2-p1-a1`，不引入数据库迁移。

- 优化 Folders / Libraries 标题的矢量箭头；展开朝下、收起朝右，保留整行点击、键盘操作及展开状态和高度比例的持久化。
- 新增 Linux x64 原生服务端归档，以及 Docker linux/amd64、linux/arm64 离线镜像；每个归档附 SHA256。Linux arm64 原生包作为 Docker 内部输入构建、验证，不单独作为 Release 资产发布。
- Docker 正式镜像只消费已验收的公开 Linux 包，包含程序、版本标记和 Rust 依赖许可证，不复制工作资料库、设置或 Git 元数据。镜像使用 UID/GID 10001、只读根文件系统及只读音频目录，运行数据保存在独立 `/data` 卷。
- 容器使用镜像替换更新，禁用程序内二进制更新。Mac arm64 与 Linux x64 原生服务端沿用管理页面更新；Windows 客户端与 Mac 客户端的更新方式不变。
- 发布资产从 0.8.3 的六项扩展为十二项；既有版本的标签、归档和历史校验规则保持原样。

Linux 与 Docker 的构建、离线导入、Compose 和验收流程见 [Linux 与 Docker 交付](Rust_Linux_Docker_Release.md)。正式包的最终提交、实测平台、beta 与人工验收结果以对应 GitHub Release 说明和产物为准。

# 0.8.3：目录分区与 macOS 客户端

程序 0.8.3 | API /api/v1/ | schema 8 | 快照兼容 3..=8

以已发布的 `v0.8.2` 为基线，完整格式保持 `ale-s8-cld1-t3-m2-p1-a1`。
本次不引入数据库迁移；Metadata、源 WAV 和服务器接口沿用现有契约。

- 左侧 Folders 与 Libraries 标题支持独立展开、收起；收起一侧后，另一侧利用剩余空间。
- 双展开时可拖动中间分隔条调整高度，也可在分隔条获得焦点后用上下方向键微调。
- 自动保存展开状态与高度比例，保留当前目录和音效库筛选、选择及目录滚动位置；
  小窗口和界面缩放下为两个展开列表各保留至少两行。
- 新增 macOS arm64 客户端适配与 `.app` 打包流程，包含桌面、CLI、图标、公共文档和
  许可证；初期通过下载 ZIP、关闭应用并替换 bundle 的方式手动安装和更新。
- Mac 素材库、设置、日志和缓存默认位于 `~/Library/Application Support/AudioLibExplorer/v1`；
  经包标记验证的 beta/rc 使用独立 `beta` 目录。显式 `AUDIOLIB_DATA_DIR` 最优先，
  `AUDIOLIB_BETA_DATA_DIR` 仅覆盖合法 beta 包；损坏或不匹配的标记拒绝启动。
  保留已配置的资料库路径，不自动迁移或删除此前未发布开发版的数据。
- Mac 文件夹、打开和保存窗口使用系统原生选择器；取消后保留原输入。支持访达定位
  本地文件和打开文件夹，启动失败提供可见错误提示。
- Mac 音频设置使用 CoreAudio；导入 ASIO 设置时改用默认 CoreAudio 输出，并重置
  设备、缓冲和采样率策略。CoreAudio 的可恢复 `Xrun` 计入欠载，`RealtimeDenied`
  不再直接中断播放；设备丢失、流失效等真正错误仍保留失败状态。
- Mac 原生退出（App 菜单、Cmd+Q、Dock）进入现有关闭流程，在退出前保存排队的
  Metadata 草稿并等待扫描和文件选择工作安全完成；新增隔离合成库的原生退出持久化回归。
- 发布资产由原先两个包、四个文件扩展为 Windows 客户端、Mac 客户端、Mac 服务端
  三个包及各自 SHA256，共六个文件。Mac 客户端包名为
  `AudioLibExplorer-<tag>-macos-arm64.zip`；历史版本资产规则及既有更新器边界保留。

Mac 客户端首次分发使用的默认签名为 ad-hoc，未经 Apple 公证。侧栏交互已完成人工
界面验收；Mac 候选的原生构建、签名、隔离启动、退出持久化与包边界验收已经实际
通过。提交对应的开发验证见[验证报告](Rust_Validation.md)；正式包的最终提交、
beta 验收与发布结果以对应 GitHub Release 的说明和产物为准。

# 0.8.2：交叉索引导航修复

程序 0.8.2 | API /api/v1/ | schema 8 | 快照兼容 3..=8

完整格式保持 `ale-s8-cld1-t3-m2-p1-a1`，Metadata 仍为原始 JSON TEXT，
不压缩 Metadata。已使用 0.8.1 的 schema 8 库无需新增数据库迁移；
更早库和快照仍按原有受保护迁移及 owned copy 升级规则处理。

变更：

- 修复旧客户端升级时被新增文档阻止的问题。Windows 客户端及服务器包保持
  v0.7.8 更新器可识别的文件布局，新增专题文档使用在线链接；打包前校验冻结的
  旧版本文件清单，避免再次生成当前更新器接受、旧更新器拒绝的发布包。
  原 v0.8.1 包的 `Non-public update file` 故障及手动升级方式见[自动更新](Rust_Auto_Update.md)。
- 修复“搜索关键词 → 选择音效库 → 全部声音”后文件夹索引仍限于旧音效库的问题。
  全部声音同步清除路径和 Album 范围，以及保留在任务中的失效专辑绑定；
  保留关键词、标签、采样率等其他过滤条件，收藏入口保留其收藏条件。
- 全部声音的高亮同时检查路径和 Album 筛选，避免仍限定某个音效库时显示成全库状态。
- 修复小窗口中音效库列表挤掉文件夹索引的问题。目录树和音效库列表共享侧栏高度，
  各自保留可见行及独立滚动区域；无需最大化窗口即可继续交叉索引。
- 修复无效时长等筛选阻止检索后，切换路径、音效库或全部声音再恢复搜索任务时，
  将旧范围的结果缓存误用于新范围的问题。显式导航先失效旧结果缓存，恢复失败的保护仍保留。
- 本地库与服务器模式的只读快照使用相同导航规则。目录索引保留关键词和音效库条件、
  忽略自身目录范围；音效库索引保留关键词和目录条件、忽略自身音效库范围。
  不改写源 WAV、Metadata 或服务端协议。

交叉索引修复候选 `2b6d57e` 的本地库与只读快照回归、58 项相关测试、严格 Clippy、
默认 ASIO 构建及固定安装验证已有独立记录；0.8.2 的完整发布校验、beta 人工验收和
最终产物验证单独记录，不以候选版本报告替代。见[验证报告](Rust_Validation.md)。

# 0.8.1：共享专辑、封面与播放修复

程序 0.8.1 | API /api/v1/ | schema 8 | 快照兼容 3..=8

完整格式：`ale-s8-cld1-t3-m2-p1-a1`；必需 feature 包括 `album_entities=1`。
Metadata 压缩暂缓，新库、扫描、迁移输出与新服务端快照保持原始 JSON TEXT；
快照外层 zstd 传输保留。

变更：

- 专辑成为共享实体，WAV 通过 `album_id` 归属。优先使用内嵌 Metadata 的 Library；
  缺失时使用已注册扫描根下第一层音效库目录，根中直接存放的 WAV 使用根本身。
  支持的根目录重新关联保留实体身份；同一共享专辑的封面按 WAV 的直接父目录区分，
  同目录成员共享图片，其他子目录独立取图，不拆分 Album 实体。
- 本地与服务器模式支持显示开关、指定/替换图片、隐藏、恢复内嵌封面、重新加载和
  清理生成缓存。自定义图片与隐藏标记属于本机偏好，独立于可丢弃缓存；图片不进入
  数据库、FTS5 或快照，操作不修改源 WAV。手动选择也按当前专辑的直接父目录隔离；
  旧全专辑选择保持只读继承，目录显式指定或隐藏优先，恢复目录默认会忽略旧继承，
  使用该目录的内嵌图片，不删除其他目录仍可继承的偏好。旧选择冲突保留原文件。
- 服务端按可选 `album_folder_artwork=1` 能力提供目录封面，校验库身份、专辑 namespace、
  目录身份和 ETag；仍使用 schema 8，不引入 schema 9。旧来源只使用当前直接目录内的
  受控候选，客户端生成的旧快照实体 ID 不冒充服务端 ID，也不借用其他目录的封面。
- 支持有效 BOOM SMED Picture 标签中的内嵌图片，保留 ID3 APIC 兼容；同名 Library 的
  Construction Kit 与 Designed 可显示各自不同的海报，SMED-only 成员也可提供目录封面。
- Metadata 保留原始 JSON TEXT、未知字段及字节内容，公开 DTO/API 不变。
  旧实验 `a1-z1` 保留严格只读解码；其本地/服务端写入口先通过 owned 转换恢复 TEXT，
  保持库 namespace、专辑身份和引用。旧压缩比例及性能结果仅为已暂缓方案的历史证据，
  本候选将重新验收；详见[共享专辑与存储](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Shared_Albums.md)。
- 库 namespace 和专辑高水位防止错误继承封面：备份/恢复保留身份并避免复用已分配 ID；
  显式导入为另一个库时使用新 namespace。旧本地偏好 alias 仅记录明确的本地旧库升级。
- 搜索任务保存专辑语义和 namespace，服务器升级后先重绑再检索；失效条件保留并提示
  重新选择，重号专辑不会被误选。个人工作区无损读取格式 1、保存为格式 2，旧程序
  会拒绝新格式，避免忽略新增过滤条件；日常用户数据目录仍保持 `v1`。
- 修复封面任务的来源与关闭竞态、缺失成员重新选择、重新关联及搜索刷新后的封面恢复。
  手动写入按接收时的目标完成，真实保存错误可见；慢 WAV/HTTP 不占住缓存锁。
- 播放回归覆盖虚拟 HTTP 源经过真实引擎 Load/Seek/Stop。先前调查确认旧服务曾等待
  macOS 文件/网络宗卷授权；部署继续保留持有授权的既有 launcher。

升级与回退：

- 本次发布基线是最后实际公开的 `v0.7.8`；`v0.8.0` 的 Release 已撤回，原标签保留不移动。
  `check-release.ps1` 对 0.8.1 默认使用 `v0.7.8`，显式 `-BaseTag` 仍优先；未来版本规则不变。
- schema 6/7 可写库通过受保护的备份、owned candidate、完整业务摘要和原子发布迁移到 8。
  Metadata、标注、播放列表、扫描历史和未知扩展数据保留，不重扫源 WAV。
  不支持的未知结构依赖会明确阻断，原始库保持不变；已创建的回退备份保留。
- schema 3–7 远程镜像在客户端 owned copy 中升级为共享专辑投影；原始只读快照不变。
  原生 schema 8 实体身份保持，旧服务跨修订投影按语义继承本地身份。
- 回退旧程序必须同时恢复对应旧 schema 备份；不能只替换二进制继续打开 schema 8 库。
  自定义封面图片仍不包含在数据库备份中。

先前 0.8.0 开发构建的实际 Mac 升级、真实封面和 WASAPI 播放证据保留在
[验证报告](Rust_Validation.md)与[播放调查记录](../docs/Playback_Fix_20261027.md)中。
0.8.1 的构建、产物验证与发布状态单独记录，不以旧构建证据替代本次验收。

# 0.8.0

程序 0.8.0 | API /api/v1/ | schema 7 | 快照兼容 3..=7

新增：按音效库/专辑归类（文件内嵌 Library）、侧栏音效库列表与封面缩略图、`albums` / `search --album` 命令。

# AudioLib Explorer release notes

## 0.6.5: update check and portable package fixes

Release identity: program `0.6.5` | API `/api/v1/` | schema 6 | snapshot format
`ale-s6-cld1-t3-m2-p1`. No database migration; schema and snapshot formats are unchanged.

Versions: program 0.6.5 | API /api/v1/ | schema 6 | snapshot compat 3..=6

Upgrade notes:

- 0.6.5 is a patch release compatible with 0.6.x peers (`>=0.6.0, <0.7.0`); schema and
  API are unchanged, so client and server can be upgraded independently.
- The 0.6.4 Windows portable ZIP used backslash entry names and could not be installed by
  the auto-updater; 0.6.5 packages install normally from 0.6.3 and 0.6.4.

Changes:

- Updater: an empty release list is retried and then reported as an error instead of
  "up to date"; a newer release skipped for a missing or invalid package is reported
  explicitly; successful checks are logged (#62).
- Packaging: the Windows portable ZIP is written entry by entry with `/` separators, and
  validation rejects backslash, directory, `..` and duplicate entries (#63).

## 0.6.4: preview volume dB label live refresh fix

Release identity: program `0.6.4` | API `/api/v1/` | schema 6 | snapshot format
`ale-s6-cld1-t3-m2-p1`. No database migration; schema and snapshot formats are unchanged.

Versions: program 0.6.4 | API /api/v1/ | schema 6 | snapshot compat 3..=6

Upgrade notes:

- 0.6.4 is a patch release compatible with 0.6.x peers (`>=0.6.0, <0.7.0`); schema and
  API are unchanged, so client and server can be upgraded independently.

Changes:

- Desktop: the preview volume bar's dB readout now refreshes live while dragging
  (and shows `-inf dB` at 0), with a regression test (#58).

## 0.6.3: startup auto-update, WASAPI idle release, beta release flow

Release identity: program `0.6.3` | API `/api/v1/` | schema 6 | snapshot format
`ale-s6-cld1-t3-m2-p1`. No database migration; schema and snapshot formats are unchanged.

Versions: program 0.6.3 | API /api/v1/ | schema 6 | snapshot compat 3..=6

Upgrade notes:

- 0.6.3 is compatible with 0.6.x peers (`>=0.6.0, <0.7.0`); client and server can be
  upgraded independently.

Changes:

- Desktop and server: check GitHub Releases at startup and apply updates with
  verified downloads, staged rollout and startup recovery (#53).
- Audio: the WASAPI output stream is released when preview is idle, so other
  applications and DAWs can use the device in exclusive or shared mode (#54).
- Release flow: beta tags and pre-releases, side-by-side beta install, and a scripted
  beta smoke test (`tools/beta-smoke.ps1`) (#51, #52).
- Desktop: the preview volume dB label refreshes live while dragging (#50).
- Server: waveform bundle supports `If-None-Match` (304); unauthenticated requests
  still get 401 (#49).

## 0.6.2: snapshot download and low-disk fixes, clearer client errors

Release identity: program `0.6.2` | API `/api/v1/` | schema 6 | snapshot format
`ale-s6-cld1-t3-m2-p1`. No database migration; schema and snapshot formats are unchanged.

Versions: program 0.6.2 | API /api/v1/ | schema 6 | snapshot compat 3..=6

Upgrade notes:

- 0.6.2 is compatible with 0.6.0 and 0.6.1 peers (`>=0.6.0, <0.7.0`); client and
  server can be upgraded independently.
- v0.6.1 shipped only the macOS server package; the Windows client package resumes
  with 0.6.2.

Changes:

- Server: fixed an occasional 500 on the first waveform bundle download (#43).
- Server: when disk space is low, idle snapshots of old revisions are evicted before
  the server answers 507 (#45).
- Server: `/library` now reports only the database file name, not its path (#46).
- Client: tolerates a missing `LibraryInfo.database`; clearer messages for
  `HOST_FORBIDDEN`, `ADMIN_LAN_ONLY` and 429 rate limiting (showing `Retry-After`);
  the `.local` host-name hint is shown only for 401 responses that did not come from
  the AudioLib server (#44).
- Desktop: the list-waveform worker releases its database connection when idle,
  fixing a Windows test flake (#47).

## 0.6.1: server hardening, waveform bundle cache and patch compatibility

Release identity: program `0.6.1` | API `/api/v1/` | schema 6 | snapshot format
`ale-s6-cld1-t3-m2-p1`. No database migration; schema and snapshot formats are unchanged.

Upgrade notes:

- **One full snapshot download after upgrading from 0.6.0.** Snapshot ETags changed
  from weak `W/"<library_id>-r<revision>"` to strong `"snapshot-<UUID>"`, so the
  first sync re-downloads the library once; later syncs are incremental as before.
- **Clients and server must both be 0.6.1.** The version handshake is mutual and
  0.6.0 only accepts an exactly matching 0.6.0 peer, so 0.6.0 and 0.6.1 cannot connect
  in either direction. From 0.6.1 on, patch releases of the 0.6 line accept each other
  (`>=0.6.0, <0.7.0`), so later 0.6.x upgrades need not be simultaneous.
- **`manage_mode` now defaults to `lan`** (was `all`). Without an explicit setting,
  scans and deletions are only allowed from the LAN, even with a token.
  Set `manage_mode = "all"` to keep the previous behaviour.

Changes:

- Server: new **403 `HOST_FORBIDDEN`** for tokenless LAN requests whose Host is not
  an IP, a LAN host name or a configured address (DNS-rebinding protection);
  tokenless LAN access by IP or LAN host name is still allowed.
- Server: new **429 `AUTH_RATE_LIMITED`** after 60 failed authentications from one
  IP within 60 seconds. 500/507 error messages no longer include server paths.
  Each listener accepts at most 1024 connections. Tokens must be at least 32 characters.
- Waveform bundle downloads are cached.
- Snapshot downloads abort a read that stalls for 90 seconds and resume with
  `Range`; there is no limit on total transfer time.
- Client UI review fixes; editing context is preserved.

## 0.6.0 local iteration: mirror cache management

Based on main `e90596d`; local delivery does not publish a tag or deploy a server.

- Server Settings → Cache and export adds **Open mirror folder** for the selected
  server's existing, identity-matched snapshot. It remains available during sync.
  Saved LAN/external profiles in the same group can share a known matching library.
- Startup housekeeping reclaims known stale top-level temporary files across
  cached libraries. Generated old mirrors are pruned after a successful swap.
- Cleanup uses nonblocking OS leases and preserves the opened/current mirror,
  resumable downloads, protected recovery jobs, manual backups and unknown files.
  Replaced-library cleanup no longer recursively removes arbitrary contents.

Exact local validation, package and installation identities are recorded in
[Rust validation](Rust_Validation.md).

## Unreleased: resumable remote database sync

This iteration is based on `origin/main` at `d64c0a7` (workspace version 0.6.0),
including its Waveform management, Metadata publication ordering and keyboard
audition fixes. It is a local development build; it does not replace a published
tag or deploy the production server. Older candidate sections below are history.

- Snapshot downloads retain compressed checkpoints and resume after interrupted
  connections or client restarts while continuing to decompress during transfer.
- Both sync pages expose Pause, Continue Sync and Restart Download. A deliberate
  pause persists through startup, polling and write reconciliation.
- A 30-second network stall triggers bounded retries. Exhaustion retains the
  checkpoint; authentication, corrupt data and disk errors stop automatic retries.
- Matching client/server builds use fixed snapshot IDs, strong ETags and byte
  ranges. Expired objects or a server restart safely require a fresh snapshot.
- CLI `remote sync` reuses checkpoints; `--restart` discards the current endpoint's
  transfer. Snapshot formats remain subject to the shared compatibility catalog and protected migration; schema 6 support is retained.

See [remote mode](Rust_Remote_Mode.md) for protocol, disk-space and recovery details.

## 0.6.0: Plan B storage and shared migration

Release identity: program `0.6.0` | API `/api/v1/` | schema 6 |
snapshot schema boundary 3..=6, subject to the complete format/capability catalog.
This is the main baseline for the resumable-sync iteration above. Measurements
and candidate status below describe the earlier Plan B validation; current local
delivery evidence is recorded in [Rust validation](Rust_Validation.md).

- Plan B removes six persisted derived-token columns while retaining tokenization
  revision 3, all twelve logical FTS5 columns, full positions and document lengths.
  Schema 6 uses contentless-delete storage and the pinned SQLite statistics fixes.
- Client, CLI and server share compatibility diagnosis and the owned
  inspect/plan/apply/resume/verify/rollback executor. Version differences alone do
  not authorize rebuilding a library. Unknown formats block unsafe use.
- Migration preserves stable IDs, Metadata, annotations, references, scan history
  and unknown supported extensions. The explicitly authorized retirement removes
  `play_history`, `audio_files.play_count` and `audio_files.last_played_at`.
  Original playback data remains recoverable in the complete pre-upgrade backup.
  The p1 format declares play_history_retired=2; earlier unpublished p0 is read-only
  compatible and migratable.
- Playback events live only in client-local history.db, default 5000 per library.
  Local legacy history imports after backup and before retirement; server/snapshot
  history is not imported. Cache replacement leaves this independent store intact.
- AudioFile JSON/CSV no longer contain play_count; attributes no longer append
  last_played_at. New clients ignore these extra fields in old server responses.
  Existing server playback routes keep validation and no-write behavior. Deploy
  clients before servers: old binaries require the removed JSON counter field.
- Download/decompression counters and migration phases/counts reach the UI.
  Invalid checkpoints stop resumption; process-crash recovery is tested for copy,
  indexing, publication and rollback. Cache retries preserve the previous snapshot
  and leave abandoned jobs intact for owner review.
- The CR branch incorporates released 0.5.0, retains its Waveform cache management
  and same-revision replies, and explicitly tests both 0.4.11 and 0.5.0 legacy peers.
  It rejects stale/unbound snapshots, checks UPDATE OF retirement dependencies,
  binds recovery to actual tokenizer sources and admits settings mutations through
  the same peer-catalog check. See [the CR record](api/Search_Storage_B_CR.md).
- Full-copy validation reduced the 909,342-record library by 45.572%, with complete
  rollback and search-result checks. First-use sorting still has a reproducible
  latency regression despite passing measured P95 targets; resolve its release
  acceptance before deploying this draft candidate.

See the [migration contract](api/Library_Migration_Compatibility.md),
[v6 iteration and acceptance](api/Schema_V6_Iteration.md) and
[CR validation record](api/Search_Storage_B_CR.md) for exact evidence and
remaining limits. Database timings do not substitute for the separate GUI or Mac
acceptance work recorded below.

## Previous unreleased baseline: upgrade recovery and integrated client/server

This candidate includes `origin/main` at `71bb1fd`, client integration at `c50bd27`
and server integration at `3eef19c`, followed by the upgrade and shutdown repairs.
The previously rejected `47ddd03` through `c0e8af9` builds must not be published.
That earlier baseline reported 0.4.11 during preparation; publication requires a new
shared program version and matching tag. Do not replace the published v0.4.11 tag.

- Existing writable libraries upgrade search text and FTS5 in committed batches,
  resume after interruption and retain one validated rollback backup. Local startup
  displays bilingual progress; server health reports the current phase and committed
  row count. Business endpoints remain unavailable until the offline upgrade finishes.
- Upgrade batches and graceful server shutdown check WAL truncation. Shutdown stops
  list Waveform workers, drains scans and snapshot jobs, then releases owned cached snapshots.
  Active downloads retain their files until their last reference is released.
- The latest client and server list Waveform changes are included, along with the
  current toolbar/focus fixes. ARM64 BM25 comparisons retain exact result-order checks
  while accepting the documented two-ULP numerical tolerance.
- Libraries, Metadata, annotations, scan history and preferences are preserved.
  API `/api/v1/`, schema 5 and snapshot compatibility 3..=5 remain unchanged.

Validation and build identity are recorded in [Rust validation](Rust_Validation.md).
The original Mac's performance rerun is deferred at the user's request. Search latency
has not met the complete **P95 latency <500ms for 1M+ records** acceptance criteria;
the recorded slow samples and pending full GUI matrix remain open. See
[known issues](Rust_Known_Issues.md) and [upgrade recovery](Rust_Upgrade_Recovery.md).
This preparation does not publish a GitHub Release or deploy the service on port 8790.

## Published v0.4.11

Release 0.4.11; the client, CLI and server share the same program version. Patch release on top of the published v0.4.10.

Versions: program 0.4.11 | API /api/v1/ | schema 5 | snapshot compat 3..=5

## Changelog

### Click-free seek, pause and stop

- The fade at seek, pause, stop and source-load boundaries is now an 8 ms raised-cosine envelope whose length is computed from the output device's sample rate (at least 32 frames), so both ends of the fade have zero slope.
- The previous fixed 32-frame linear ramp lasted only 0.67 ms at 48 kHz and 0.33 ms at 96 kHz; its corners produced an audible click on every seek, especially at 96 kHz.
- The audio callback still only reads atomics and preallocated state. Volume, channel routing, source positions and playback sessions are unchanged.

### Column widths stay after dragging a divider

- Dragging a column divider shortly after clicking it no longer snaps the column back to its default width on release, and the default width is no longer saved in that case.
- Only a genuine double-click, where neither click drags the divider, restores the column's default width.

### Seek loopback diagnostic

- The `seek-loopback` diagnostic example gained `--signal sine`, `--tone-hz`, `--source-rate` and `--source-channels` options, so it can exercise the resampling and multichannel paths, and it reports envelope corners (second difference) next to the test tone's own slope and curvature.

## Known issues

- The first Play after the output device is opened still uses a 5 ms linear volume ramp.

## Compatibility

API paths, the database schema and the snapshot compatibility range are unchanged, and the settings file format is unchanged. Upgrading keeps libraries, Metadata, annotations, scan history and preferences; runtime state stays in `%LOCALAPPDATA%/AudioLibExplorer/v1` or the existing explicit data directory. The Windows x64 portable package includes WASAPI and ASIO.

The Release provides the Windows x64 client portable package and Windows x64 and Linux x64 standalone server packages, each with a SHA-256 file and an allowed-field validation report.
