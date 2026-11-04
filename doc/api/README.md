# AudioLib 远程 API（v1，讨论稿）

> 状态：讨论稿。里程碑 1（`/health`、`/library`、`/library/snapshot`、`/roots`）和里程碑 2（`/audio/{id}`、`/audio/{id}/stream`，以及启动期 `/health` 状态 + 503 `UNAVAILABLE`）、里程碑 3（metadata 写入：`PATCH /audio/{id}/metadata`、`POST /audio/metadata/batch`、`POST /audio/{id}/plays`，§6.3）以及波形（§9）、试听预览 `/audio/{id}/preview`（§8.1）、内嵌封面 `/audio/{id}/artwork`（§8.2）、远程扫描 `/scans`（§6.6）、历史列表 `/history/...`（§6.7）、从库中移除记录/文件夹 `POST /audio/remove`、`DELETE /folders/{id}`（§6.8）以及库管理权限 `manage_mode` + `GET /capabilities`（§6.9）及客户端远程目录浏览与根目录新增/改路径/删除（§6.10）已在 `server/audiolib-server` 实现；其余端点仍为计划，等服务端确认、用户拍板后再定稿。改任何路由或字段先在「AudioLib 开发组」群里通知对方。

## 1. 目标与边界

当前 0.8.1 候选使用 schema 8 / `ale-s8-cld1-t3-m2-p1-a1`（`album_entities: 1`），
专辑封面接口见 §8.2。新库、迁移输出及新服务端快照的 Metadata 物理列保持原始
JSON TEXT，压缩暂缓；外层快照 zstd 传输保留。客户端与服务器须配套升级。
旧实验 `a1-z1` 仅保留严格只读兼容，不作为可写目标。旧快照只在客户端自有副本迁移，
源格式与投影格式分别保存。详见[共享专辑与存储](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Shared_Albums.md)。
下段是此前续传迭代的历史身份和验收范围。

当前续传迭代基于 main `d64c0a7`，程序版本为 `0.6.0`，保留 schema 6 / p1、共享格式协商和受保护迁移。固定快照及 Range 协议见 §6.1；本轮验证见 [Rust 验证报告](../Rust_Validation.md)。生产服务器不在本轮部署范围内，工作库没有被验收脚本打开。以下链接保留此前数据库迭代的设计与历史证据：
[方案 B 存储设计](Search_Storage_B.md)、
[版本兼容与迁移契约](Library_Migration_Compatibility.md)、
[历史全库实验](Search_Storage_B_Validation.md)、
[共享迁移器全库与回滚实测](Search_Storage_B_Integrated_Validation.md)、
[实现与验收边界](Implementation_B_Status.md)。
CR、修复及历次隔离证据见 [方案 B CR](Search_Storage_B_CR.md)；p1、本机历史和首次排序修复的原构建身份与测量见 [schema v6 合并升级](Schema_V6_Iteration.md)。本轮未重跑全库迁移或广泛性能验收；只补受影响的验证与五类排序抽查。

历史候选 `c81eca2` 已合入 main `c90c0fe`（0.5.1，含方向键试听修复），当时三端版本为 `0.6.0-dev.1`。旧端例外只接纳精确的 0.4.11 / 0.5.0 / 0.5.1，未知 patch 和未列入目录的预发布仍拒绝；新客户端可连接这些旧服务端，旧客户端仍不能读取新 p1 快照或执行 p1 库写入。该候选的本地验证和部署边界保留于上述记录。用户部署后另行通知，再按 [局域网验收计划](Schema_V6_LAN_Acceptance.md) 核验实际服务。

- 服务器模式 = 一个实现 `audiolib_core::DataProvider` 的远程提供者（`crates/audiolib-remote`）+ 服务端进程（`server/audiolib-server`）。UI / CLI 不感知本地还是远程。
- 服务端直接复用 `audiolib-core` 的 DTO（`AudioFile`、`Folder`、`SearchRequest`、`SearchPage`、`MetadataPatch`、`ScanProgress`、`LibraryStats`、`LibraryInfo` 等），JSON 字段名与 serde 序列化结果一致，remote 端可以直接反序列化。
- 源音频永远只在服务端磁盘上；客户端通过 WebDAV（见 §7）或流端点（见 §8）读取，不直接打开服务端路径。
- v1 不做：备份/还原、维护（integrity/FTS 重建）、relink、JSONL/CSV 导入导出、多用户权限。这些在远程模式下返回 `UNSUPPORTED`，留在服务端本机 CLI 操作。`GET /library/snapshot` 是只读导出，不属于上述备份/还原。
- 局域网内直接用 HTTP（`http://192.0.2.10:8790`，默认不需要 token）；外网经 HTTPS 反向代理访问（如 `https://audiolib.example.com:8443`，需要 token），见 §2.2–§2.4。token 在服务端管理页 `/admin` 生成和撤销（§2.3）。

## 2. 基础约定

- Base URL：`<服务器地址>/api/v1`。服务器地址由用户填写：局域网为 `http://<host>:8790`（服务端默认端口 `8790`，避开 Mac 上已占用的 3000/2222），外网为反代的公网地址，形如 `https://<域名>:<端口>`，可带路径前缀（如 `https://example.com/audiolib` → `https://example.com/audiolib/api/v1`）。服务端在 `/health` 的 `public_url` 里给出管理员配置的公网地址（§6.2）。
- 编码：请求/响应 `application/json; charset=utf-8`，快照和音频流除外。
- JSON 修改请求（含管理页 JSON API）必须声明 `Content-Type: application/json`（可带 `charset`）；缺少该头或使用 `text/plain` / 表单类型返回 415 `JSON_CONTENT_TYPE_REQUIRED`。无 JSON 正文的播放计数、扫描控制、根检查和 DELETE 不要求该头。
- 浏览器发来的修改请求（包括正文被忽略的 `POST /audio/{id}/plays`、根检查）若带 `Origin`，必须与请求 `Host` 的主机、端口及实际监听器协议一致，否则返回 403 `ORIGIN_FORBIDDEN`。HTTP 监听器只接受同源 `http`，原生 TLS 监听器只接受同源 `https`；不信任转发头的协议声明，无法验证监听器、重复/无效 Origin 也拒绝。管理页沿用 403 `ADMIN_HEADER_REQUIRED`。无 Origin 的原生客户端照常使用。
- 鉴权：按**鉴权模式**（§2.2）决定，默认 `external`：**局域网直连不需要 token，请求里带的 `Authorization` 头被完全忽略**（有效、已撤销或乱写的 token 都不会导致 401）；经反向代理或来自公网的请求需要 `Authorization: Bearer <token>`。`GET /health` 永远不需要 token。客户端可以始终发送已保存的 token，各种模式都兼容。
- 时间：Unix 秒（与现有 envelope 的 `timestamp` 一致）；时长 `duration` 为秒（f64）。

### 2.1 成功与错误

成功直接返回 DTO 本体（不再包 `success/data` envelope，HTTP 状态码表示成败）。错误统一：

```json
{ "error_code": "VALIDATION", "message": "Search limit must be 1..=10000" }
```

| `audiolib_core::Error` | HTTP | `error_code` |
|---|---|---|
| `Validation` / 路径参数非法（如 `/audio/abc`）/ 请求体不是合法 JSON 或字段类型不对 | 400 | `VALIDATION` |
| 需要 token 时（§2.2）token 缺失、错误或已撤销 | 401 | `UNAUTHORIZED`（附 `WWW-Authenticate: Bearer`；缺失和错误同一个码，只有 `message` 不同） |
| 管理页 `/admin` 经反代、从公网或用非局域网主机名访问（§2.3） | 403 | `ADMIN_LAN_ONLY` |
| 管理页的修改请求缺少 `X-AudioLib-Admin: 1` 或来自其他源（§2.3） | 403 | `ADMIN_HEADER_REQUIRED` |
| 库管理（扫描及其暂停/继续/取消、记录/文件夹移除、远程目录浏览和根目录管理、列表波形任务开始/暂停/继续/停止（§9.5）、`PATCH /settings`（含 `pregenerate_list_waveforms`、`list_waveform_rate`））被 `manage_mode` 拒绝：`lan` 模式下的外部请求，或 `off` 模式下的任何请求（§6.9–§6.10）；先过鉴权，缺 token 仍是 401 | 403 | `MANAGE_FORBIDDEN` |
| `NotFound` / 未知路由 / 音频文件缺失或不在配置的根下 | 404 | `NOT_FOUND` |
| 路由存在但方法不对（如 `POST /library`） | 405 | `METHOD_NOT_ALLOWED`（JSON 错误体，保留 `Allow` 头） |
| 受通用请求超时控制的端点超过 30 秒未完成（根目录 PATCH/DELETE 例外，§6.10；目录浏览另有 15 秒上限） | 408 | `TIMEOUT` |
| `Busy`（扫描/维护/快照并发占用；已有扫描时再 `POST /scans`，§6.6；扫描进行中或暂停中、或其他进程扫描/维护同一个库时 `POST /audio/remove`、`DELETE /folders/{id}`，§6.8） | 409 | `BUSY` |
| 库修改请求缺少 `X-AudioLib-Library-Id` 或包含重复/空值；读取端点、管理页和根检查除外 | 409 | `LIBRARY_ID_REQUIRED` |
| `X-AudioLib-Library-Id` 与处理该请求的库身份不一致 | 409 | `LIBRARY_CHANGED` |
| 浏览器修改请求带非同源、无效或无法验证的 Origin | 403 | `ORIGIN_FORBIDDEN` |
| JSON 修改请求的 Content-Type 缺少或不是 application/json | 415 | `JSON_CONTENT_TYPE_REQUIRED` |
| `Cancelled` | 409 | `CANCELLED` |
| 扫描会话当前状态不允许该操作（已结束、正在取消、已进入最终对账等，§6.6） | 409 | `SCAN_STATE` |
| 列表波形任务当前状态不允许该操作（没有任务或已结束，§9.5，草案） | 409 | `JOB_STATE` |
| JSON 请求体超过 8 MiB（§6.3） | 413 | `PAYLOAD_TOO_LARGE` |
| 音频流 `Range` 无法满足（§8）、`pcm` 预览 `Range` 无法满足（§8.1） | 416 | `RANGE_NOT_SATISFIABLE`（附 `Content-Range: bytes */<长度>`） |
| `flac` 预览的 `t=` 在时长之后（§8.1） | 416 | `RANGE_NOT_SATISFIABLE`（不带 `Content-Range`） |
| `codec=pcm` 预览遇到不支持的文件（非 WAV、ADPCM 等，文件头损坏或截断，或采样率无法按 `sr` 整数倍抽取，§8.1） | 415 | `PREVIEW_UNSUPPORTED_FORMAT` |
| 远程不支持的操作 | 501 | `UNSUPPORTED` |
| 库尚未打开（启动中）或打开失败、正在重试（§6.2） | 503 | `UNAVAILABLE`（附 `Retry-After: 5`）；**只用于这一种情况** |
| 音频流/预览/波形/封面对应文件所在的库根目录、或 `POST /scans` 要扫描的根离线或暂时无响应（§6.4、§6.6、§8.2） | 503 | `ROOT_OFFLINE`（附 `Retry-After: 5`，`message` 为 `Library root "<根名>" is offline: <原因>`） |
| 音频流并发达到上限 `max_streams`（§8），预览（`pcm`/`flac`）并发达到独立的上限 `max_previews`，或 `flac` 预览等待 0.5 秒仍无转码名额 `max_transcodes`（§8.1）；`POST /waveforms` 同时处理的请求超过 4 个（§9.1） | 503 | `BUSY`（附 `Retry-After: 1`，几种情况形状完全相同；快照并发仍为 409 `BUSY`） |
| 快照还在准备、等待超过 `snapshot_wait_secs`（§6.1） | 503 | `BUSY`（附 `Retry-After: 5`；准备在后台继续，重试即可拿到；与音频流 `BUSY` 同码，靠 `Retry-After` 和端点区分） |
| `flac` 预览所需的 ffmpeg 不存在或无法启动（§8.1） | 503 | `TRANSCODER_UNAVAILABLE`（不带 `Retry-After`） |
| 写快照/备份副本前检查所在卷的剩余空间不足（§6.1；需要的字节数 + max(64 MiB, 2 %) 余量）或系统报告磁盘已满 | 507 | `INSUFFICIENT_STORAGE`（`message` 含目录和所需/剩余 MiB；腾出空间后重试，无 `Retry-After`） |
| `Database` / `Audio` / `Io` | 500 | `DATABASE` / `AUDIO` / `IO` |
| 服务端内部故障（如后台任务崩溃） | 500 | `INTERNAL` |

remote 端把 `error_code` 还原成对应的 `Error` 变体，保证 GUI 现有错误提示逻辑不变；`INTERNAL` 按 `Database` 处理即可，`RANGE_NOT_SATISFIABLE` 只会出现在音频流和预览上（§8、§8.1），`BUSY` 按状态码区分快照（409）和音频流/波形（503）。各种 503 按 `error_code` 区分，不要匹配 `message` 文字：

- `BUSY`（503）：音频流/预览名额已满，按 `Retry-After` 稍后重试（§8、§8.1）。
- `TRANSCODER_UNAVAILABLE`：只出现在 `/audio/{id}/preview?codec=flac` 上，服务端没有可用的 ffmpeg；重试无用，改用 `codec=pcm` 或 `/stream`。
- `UNAVAILABLE`：**整个库**暂不可用（服务端启动中或开库失败正在重试，§6.2），任何需鉴权的端点都可能返回；按“服务器暂不可用、稍后重试”处理（类似连接失败，可沿用离线缓存）。
- `ROOT_OFFLINE`：只出现在 `/audio/{id}/stream`、`/audio/{id}/preview`、`/audio/{id}/waveform`、`/audio/{id}/artwork`（§8.2）和 `POST /scans`（§6.6）上（`POST /waveforms` 不整体返回它，而是逐项 `status: "root_offline"`，§9.1），表示**该文件所在（或要扫描）的库根**离线或暂时无响应；库本身可用（元数据、快照、写入照常），客户端可显示 `message` 中的原因、查看 `/roots` 的 `status`，或用 `POST /roots/{name}/check` 立即重查（§6.4）。

500 与 507 的 `message` 使用概括文字，原始路径和内部诊断只写服务端日志；503 的 `message` 和 `/health` 不含路径（`UNAVAILABLE` 为固定文字，`ROOT_OFFLINE` 为根名 + 原因）。

连接与超时：服务端只说 HTTP/1.1；连接须在 10 秒内发完请求头（空闲的 keep-alive 连接同样在 10 秒后关闭），除 `/library`、`/library/snapshot`、`/audio/{id}/stream`、`/audio/{id}/preview`、`/audio/{id}/waveform`、`/audio/{id}/artwork`、`POST /scans`、`PATCH /roots/{name}`、`DELETE /roots/{name}` 外的端点 30 秒内未完成返回 408 `TIMEOUT`（这些端点不受通用 30 秒上限，见 §6.4、§6.6、§6.10；`/library` 通常毫秒级返回，仅在百万级库且无统计缓存时的第一次请求可能等待统计计算；`POST /waveforms` 受 30 秒上限，自身有 2 秒 Metadata 预算，§9.1）；响应还有数据要发但客户端连续 60 秒一个字节都不收（休眠、断网，或拿着响应不读）时，服务端断开该连接并释放它占用的名额；服务端停止时最多等 10 秒让进行中的请求结束，之后断开剩余连接（客户端看到连接中断或响应体不完整，按可重试错误处理）。

### 2.2 鉴权模式与 token

服务端有三种鉴权模式（`config.toml` 的 `auth_mode`，或管理页设置，管理页优先）：

| 模式 | 局域网直连 | 外部请求（经反代 / 公网对端） |
|---|---|---|
| `off` | 不需要 token | 不需要 token（只在完全可信的网络里用） |
| `external`（**默认**） | 不需要 token；自定义公网 Host 需有效 token | 需要 `Authorization: Bearer <token>` |
| `all` | 需要 token | 需要 token |

- 旧配置 `auth = true` 等同 `all`（并且启动时必须有环境变量 token）；`auth = false` 或不写时为 `external`。示例配置是 `auth = false`，所以升级后局域网客户端的行为不变（不需要 token）。
- **外部请求的判定**（满足任一即外部）：请求带有 `X-Forwarded-For`、`X-Forwarded-Proto`、`X-Forwarded-Host`、`Forwarded` 或 `X-Real-IP` 头（不论值，表示经过反向代理）；或 TCP 对端地址不在私有网段内（`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`、`fc00::/7`、`fe80::/10`；`100.64/10`、`169.254/16` 等都算外部）；或对端就是**服务端所在的本机**（见下条“同机规则”）。原生 TLS 监听（§2.3）上的公网对端也是外部；局域网对端经 TLS 直连仍算局域网。服务端不信任这些头里的地址，只看“有没有”，所以客户端伪造它们只会让自己被当作外部请求。**因此反向代理必须设置 `X-Forwarded-For`（§2.4），否则局域网内另一台机器上的反代会被当成局域网直连。**
- **同机规则**：只有**其他机器**上的局域网对端才算局域网直连。以下对端一律按外部请求处理（需要 token，`/admin` 返回 403 `ADMIN_LAN_ONLY`）：回环地址 `127.0.0.0/8`、`::1` 及 IPv4 映射的回环地址（`::ffff:127.x.x.x`）；以及 TCP 对端 IP 等于该连接本地 socket 地址 IP 的连接（即本机通过自己的局域网 IP 访问自己，如对端与本地都是服务端的局域网 IP）。原因：与服务端同机运行的反向代理（如 Lucky）把公网流量转发到 8790 时，连接来自回环或本机局域网 IP；即使反代没有加转发头，这些请求也不能免 token。因此在服务端所在的 Mac 上用 `localhost` / `127.0.0.1` / 本机 IP 访问 API 时需要带 token（`GET /health` 仍然免 token），管理页 `/admin` 需从局域网内的另一台设备打开。
- 有效 token：管理页生成的 token（未撤销），或环境变量 `AUDIOLIB_SERVER_TOKEN`（`token_env` 可改名；部署时通常由 systemd `EnvironmentFile` 或启动脚本导出它，所以它也一直有效）。生成的 token 形如 `al_` + 64 个十六进制字符；服务端只保存 SHA-256 哈希（常量时间比较），明文只在生成时显示一次。撤销立即生效。
- 认证头格式：`Authorization: Bearer <token>`（`Bearer` 不区分大小写）。`Basic` 等其他方案视为没有 token。
- 错误：需要 token 而缺失 → 401 `UNAUTHORIZED`（`message`：`A bearer token is required: this request is not from a trusted direct LAN peer`；`all` 模式为 `A valid bearer token is required`）；token 错误或已撤销 → 401 `UNAUTHORIZED`（`message`：`The bearer token is invalid or has been revoked`）。都带 `WWW-Authenticate: Bearer realm="audiolib"`。客户端按状态码和 `error_code` 判断，不要匹配 `message`。没有新增错误码（`TOKEN_REQUIRED` / `TOKEN_INVALID` 不存在）。
- token 的“最近使用时间”最多每分钟记一次（避免频繁写盘），在管理页显示。
- **Host 防护**：`trust_lan_peers = true`（默认）时，来自局域网直连对端（socket 对端 IP 为私有或链路本地地址且不是本机，含 IPv4 映射的 IPv6，且请求不带任何转发头）的免 token 请求一律放行（回环和同机对端除外，见“同机规则”），不检查 `Host`——局域网客户端可以用 IP、`<服务器主机名>.local` 等 LAN 名称，或经 NAT 回环 / split DNS 用公网域名访问。其他免 token 请求（`auth_mode = "off"` 下的非局域网对端）必须携带单一合法 `Host`，主机为 IP、LAN 名称，或已配置 `public_url` / `lan_url` 的主机名；否则 403 `HOST_FORBIDDEN`。`external` 模式下非局域网或带转发头的请求仍需 token（401）。有效 bearer 可访问其他合法主机名。注意：局域网内浏览器因此不再受 API 层 DNS rebinding 防护（`/admin` 仍单独校验 Host）；若局域网不可信，请使用 `auth_mode = "all"` 或 `trust_lan_peers = false`。
- **隐藏来源的部署**：`trust_lan_peers = false` 将全部对端按外部请求处理，并关闭免密码 `/admin`；同时使用 `auth_mode = "all"`。Docker Desktop、rootless 容器、SNAT 和未可靠保留来源的反代必须使用此配置，通过 TOML 管理。示例容器采用此策略，仅映射宿主机 `127.0.0.1:8790`。
- **失败限速**：按 socket peer IP 记录失败（不信任转发头里的地址），60 秒内 60 次失败后返回 429 `AUTH_RATE_LIMITED` 和 `Retry-After: 60`。最多保留 1024 个来源；日志记录来源，不记录 bearer。代理后的客户端共用代理 IP 的限额；限速期间管理页生成的 token（256 位随机，无法猜中）仍然有效，环境变量 token 和错误 token 继续返回 429。

### 2.3 管理页 `/admin`

浏览器打开 `http://<服务器局域网地址>:8790/admin`（例如：`http://192.0.2.10:8790/admin`；必须从局域网内的另一台设备打开，服务端所在的 Mac 本机经 `127.0.0.1` 或本机 IP 访问都按外部请求处理，返回 403，见 §2.2 同机规则）。页面是编译进服务端的静态 HTML + JS，不需要构建、不加载任何外部资源。功能：

- **客户端连接**：显示客户端应填写的外网地址（公网地址）和局域网地址，以及当前鉴权模式下哪种地址需要 token。局域网地址默认从网卡自动检测（私有网段的 IPv4，`en*` 优先），可在页面改写。
- **公网地址**：例如 `https://audiolib.example.com:8443`，可带路径前缀（`https://example.com/audiolib`）。必须是 `http`/`https` URL，不能含用户名密码、查询串或片段；保存时规范化（小写协议和主机、去掉结尾 `/` 和多余的 `/api/v1`）。保存后 `/health` 的 `public_url` 立即更新。
- **鉴权模式**：`off` / `external` / `all`，或恢复 `config.toml` 的默认值。
- **库根目录**：列出当前根目录（名称、路径、在线状态、文件数、缺失数、最近扫描），添加 / 移除根目录（带服务器磁盘浏览器，可浏览已挂载的 SMB 卷），对某个根目录发起增量 / 完整扫描并显示进度、暂停 / 继续 / 取消。改动立即生效，无需重启，详见 §2.3.1。
- **库管理权限**（`manage_mode`）：`lan`（默认，只允许局域网直连）/ `all`（所有通过鉴权的请求）/ `off`（全部禁止），或恢复 `config.toml` 的默认值；决定谁能远程扫描、从库中移除记录和文件夹（§6.9）。保存后立即生效。已有显式设置仍然保留。
- **访问令牌**：填名称生成（明文只显示一次，带复制按钮）；列表显示名称、生成时间、最近使用时间；撤销。最多 256 个。
- **原生 HTTPS（可选）**：证书链 PEM 路径 + 私钥 PEM 路径（绝对路径）+ HTTPS 端口（如 8791，不能等于 HTTP 端口）。保存前校验文件能加载、证书和私钥匹配；保存后**立即生效、无需重启**（服务端在与 `bind` 相同的地址上另开 rustls 监听，8790 的 HTTP 照常），页面显示监听状态或错误（如端口被占用）。停用同样即时。**本版本客户端只信任公共 CA 签发的证书（如 Let's Encrypt），自签名证书客户端不会接受。** 一般用反向代理提供 HTTPS（§2.4），不需要这个功能。

**访问限制**（本轮唯一的安全措施，没有管理密码）：

- 只接受局域网直连：TCP 对端在上面的私有网段内，且**没有**任何转发头（`X-Forwarded-For` 等，即不是经反向代理来的），原生 TLS 监听上的公网对端也拒绝；否则 403 `ADMIN_LAN_ONLY`。
- `Host` 必须是 IP 地址、`localhost`、不带点的主机名或 `.local` / `.lan` / `.home.arpa` / `.internal` 结尾的名字，否则 403 `ADMIN_LAN_ONLY`（防 DNS 重绑定：恶意网页无法借自己的域名访问管理接口）。
- 只读请求（`GET`/`HEAD`/`OPTIONS`，如 `GET /admin/api/status`、`GET /admin/api/runtime`、`GET /admin/api/tokens`）只做上面两项检查（局域网直连 + LAN `Host`），**不需要** `X-AudioLib-Admin` 头，也不检查 `Origin`。
- 修改请求（`PUT`/`POST`/`DELETE`）必须带 `X-AudioLib-Admin: 1`，浏览器带 `Origin` 时必须与 `Host` 同源，否则 403 `ADMIN_HEADER_REQUIRED`（自定义头使跨站网页无法静默发起修改）。
- 管理响应都带 `Cache-Control: no-store`、`X-Frame-Options: DENY`，页面另有严格的 `Content-Security-Policy`。

**设置的保存位置**：库文件所在目录（例如：`/var/lib/audiolib/`）下的 `admin-settings.json`（公网地址、鉴权模式、库管理权限、局域网地址、TLS）和 `tokens.json`（token 名称、SHA-256、生成/最近使用时间），原子写入，权限 0600。**不修改 `config.toml`**：`config.toml` 中的 `auth_mode`、`manage_mode`、`public_url`、`tls_*` 是默认值，管理页的设置覆盖它们（在页面上清除即回到 `config.toml` 的值）。文件损坏时启动不失败：该文件被改名为 `*.json.corrupt`，页面顶部显示警告。设置在处理该管理页请求的实例中即时生效，没有需要重启的项（页面上只读显示的 `config.toml` 资源限制除外，见 §2.3.2）。

共享同一设置目录时，受 `manage_mode` 保护的 API 在准入时取得跨实例设置锁、重读最新设置，并在同一锁内依次检查 bearer 和管理权限，随后检查库身份。设置锁被另一操作占用时先返回 409 `BUSY`，不使用旧策略放行。另一实例已收紧或放宽策略后，首次获准进入权限核验的管理请求就使用新策略；已准入请求不因之后的策略更改撤回。普通读取、音频流及非管理写入使用该实例当前的鉴权状态，不等待耗时根目录编辑的设置锁；这不承诺所有设置与 token 在所有实例间实时广播。局域网 `/admin` 入口继续使用独立的 LAN、Host 与同源检查。

管理接口（页面自己使用，JSON；错误同 §2.1）：

| 请求 | 说明 |
|---|---|
| `GET /admin/api/status` | 版本、库状态、`auth_mode`(+`_source`: `default`/`config`/`admin`)、`manage_mode`(+`_source`，同上)、`public_url`(+`_source`)、`lan_url`、`lan_url_source`、`lan_url_candidates`、`http_port`、`env_token`（是否配置了环境变量 token）、`token_count`、`tls { configured, source, state: off/starting/listening/error, port, message }`、设置文件路径、`warnings` |
| `GET /admin/api/runtime` | 只读运行状态，见下文 §2.3.2；每次请求只读内存状态 |
| `GET /admin/api/update` | 更新状态 `{state, current_version, channel, tag, release_url, message, checked_at}`；`state` 为 `disabled`/`checking`/`current`/`available`/`downloading`/`ready`/`error`；`checked_at`（新增，Unix 秒）为本进程最近一次检查完成（成功或失败）的时间，未检查过为 `null`；`skipped_newer`（新增，向后兼容）为最近一次检查中被跳过的更新版本列表 `[{tag, reason}]`（新到旧，例如更新的 release 缺少本平台有效安装包而改为提供较旧版本），无则为空数组 `[]`，开始新检查时清空；管理页在更新区显示「跳过的新版本：tag — 原因」，为空时隐藏 |
| `POST /admin/api/update/check` | 开始一次后台检查，立即回复同 `GET /admin/api/update`；已有更新操作进行中 409 `BUSY`，未启用 503 `UNAVAILABLE` |
| `POST /admin/api/update/install` | `{"tag"}`（必须等于检查得到的 `tag`）下载、校验并部署；扫描进行中 409 `UPDATE_BUSY` |
| `PUT /admin/api/settings` | `{"public_url"?, "auth_mode"?, "manage_mode"?, "lan_url"?, "pregenerate_list_waveforms"?, "list_waveform_rate"?}`，只改出现的字段；`null` 或 `""` = 删除覆盖、回到 `config.toml`（`manage_mode` 只接受 `"all"`/`"lan"`/`"off"`/`null`，其他值 400 `VALIDATION`）。注意 `false` 与 `null` 不同：`"pregenerate_list_waveforms": false` 保存一个管理页覆盖（显式关闭，`config.toml` 改为 `true` 也不生效），`null`（或 `""`）才删除覆盖、回到 `config.toml` 的值；回复同 status |
| `PUT /admin/api/tls` | `{"cert_path","key_path","port"}`，校验后保存并（重新）监听；回复同 status（最多等 4 秒拿到监听结果） |
| `DELETE /admin/api/tls` | 停用（回到 `config.toml` 的 TLS 设置，默认没有） |
| `GET /admin/api/tokens` | `{"tokens":[{"id","name","created_at","last_used_at"}]}`（不含哈希） |
| `POST /admin/api/tokens` | `{"name"}` → 201 `{"token":{…},"secret":"al_…"}`，`secret` 只出现这一次 |
| `DELETE /admin/api/tokens/{id}` | 撤销 → 204；不存在 → 404 `NOT_FOUND` |
| `GET /admin/api/fs`、`GET`/`POST /admin/api/roots`、`DELETE /admin/api/roots/{name}`、`/admin/api/scans...` | 库根目录管理，见 §2.3.1 |

原生 TLS 每分钟检查证书和私钥内容；有效续期替换只影响新握手，已有连接继续使用原会话。续期文件暂不匹配或不可读时保留旧证书，管理页 TLS 状态 `message` 与服务端日志报告原因，之后继续重试。每个 HTTP/TLS 监听器最多接纳 1024 个连接（含未完成 TLS 握手），超额连接立即关闭。

#### 2.3.1 库根目录管理（管理页专用 Web API）

> **客户端不得依赖本节接口**：它们只供管理页使用，可能随时改动，且只接受局域网直连（§2.3 的全部访问限制：非局域网 / 带转发头 403 `ADMIN_LAN_ONLY`，修改请求缺 `X-AudioLib-Admin: 1` 或跨源 403 `ADMIN_HEADER_REQUIRED`）。客户端读取根目录仍用 `GET /api/v1/roots`（形状不变），在管理页添加的根目录会出现在其中（`webdav_url` 为 `null`），移除的根目录消失。鉴权模式和 `manage_mode` 对这些接口无关（管理页本身就是服务端管理员的入口）。

**根目录的保存与生效**：添加 / 移除后，当前完整的根目录列表写入 `admin-settings.json` 的 `roots`，**此后以它为准**，不再读取 `config.toml` 的 `[[roots]]`（和其他管理页设置一样，管理页覆盖 `config.toml`；`config.toml` 里原有的根目录也可以在页面移除）。改动立即生效，无需重启，重启后保持。

**`GET /admin/api/fs`**（只读，从不写入、删除或递归扫描；每次请求只做一次 `read_dir`，在 SMB 上也很快）：

- 不带 `path`：起点列表 `{"places":[{"kind":"root|volume|home|library_root","name","path","network":true|false|null}]}`：`/`、`/Volumes/*`（`network` 由一次 `statfs` 的文件系统类型判断，`smbfs`/`nfs`/`afpfs`/`webdav` 为 `true`）、主目录、现有根目录（Windows 上是各盘符）。
- `?path=<绝对路径>[&hidden=1]`：列出该目录的子目录（默认不含 `.` 开头的隐藏目录），路径先 `canonicalize`（解析 `..` 和符号链接）：

```json
{ "path": "/srv/audiolib/music/Sounds", "parent": "/srv/audiolib/music", "network": false,
  "entries": [ { "name": "Ambience", "path": "/srv/audiolib/music/Sounds/Ambience", "is_symlink": false } ],
  "truncated": false, "wav_files": 12, "other_files": 3, "counts_truncated": false }
```

  `entries` 不区分大小写排序，最多 2000 项（超出 `truncated: true`）；指向目录的符号链接会列出并标 `is_symlink`，指向文件的不列出；`wav_files` / `other_files` 只统计该目录**直接**包含的文件（不递归，最多检查 50000 个条目，超出 `counts_truncated: true`；这时排在后面的子目录也不会列出，即使 `truncated` 为 `false`）。错误（`message` 为中文）：相对路径或空路径 / 不是目录 → 400 `VALIDATION`；不存在（如磁盘未挂载）→ 404 `NOT_FOUND`；系统拒绝读取（权限，或 macOS 隐私保护 TCC 弹窗尚未允许）→ 403 `FS_FORBIDDEN`。目录 15 秒内没有响应（磁盘休眠、网络共享无响应，或 macOS 正在等待“访问网络卷”的权限弹窗，需在服务器屏幕上点允许）→ 504 `FS_TIMEOUT`（`POST /admin/api/roots` 的路径检查同理）。卡住的系统调用在返回前一直占用一个名额（管理页独立保留 1 个，不受客户端 API 的 4 个名额占用影响，管理页浏览与添加根目录共用），名额用完时直接 503 `BUSY`（`Retry-After: 1`），不再为同一个无响应的共享叠加阻塞线程。

**`POST /admin/api/roots`** `{"name","path"}` → 201，回复同 `GET /api/v1/roots` 的一项（新根目录尚未扫描，`file_count` 为 `null`）。校验（400 `VALIDATION`，不存在 404，权限 403 `FS_FORBIDDEN`）：`name` 非空、1..=128 字符、无首尾空白、不含 `/` `\` 和控制字符、与现有根目录不重名（不区分大小写）；`path` 是已存在的绝对目录；不能与现有根目录相同、位于其内部或包含它。保存的 `server_path` 是规范化后的真实路径。

**`DELETE /admin/api/roots/{name}`** → 200：

```json
{ "name": "Music", "removed": 3, "folders_removed": 2, "revision_before": 41, "revision": 42 }
```

先让库忘记该根目录下的全部记录（与 `DELETE /folders/{id}` 相同的清理：音频记录、文件夹记录、播放历史、歌单项、波形缓存），再把它从根目录列表移除；**绝不删除任何音频文件**，重新添加并扫描即可恢复。只有真的删掉记录时 `revision` 才加 1（从未扫描过的根目录：`removed` 为 0，`revision` 不变）。有扫描在运行或暂停时 → 409 `BUSY`；不存在 → 404 `NOT_FOUND`。

**扫描**：`POST /admin/api/scans`（请求体和回复同 `POST /api/v1/scans`）、`GET /admin/api/scans/current`、`GET /admin/api/scans/{id}`、`POST /admin/api/scans/{id}/pause|resume|cancel` 与 §6.6 的同名接口完全相同，只是走管理页的访问限制、不受 `manage_mode` 限制。`GET /admin/api/roots` 返回与 `GET /api/v1/roots` 相同的列表（不需要令牌）。

#### 2.3.2 运行状态 `GET /admin/api/runtime`

只读，访问限制同其他管理接口（局域网直连 + LAN `Host`，否则 403 `ADMIN_LAN_ONLY`；`Cache-Control: no-store`）。**只读取内存中的状态和已缓存的计数**（信号量、列表波形调度器、缓存计数器、根目录监控的缓存结果、revision 跟踪器、更新管理器），不扫描、不遍历文件系统、不查询库数据库，可以频繁轮询；管理页每 5 秒刷新一次，标签页隐藏时暂停。库启动中或打开失败时 `library` 为 `starting` / `error`，库相关字段为 `null`、`roots` 为空，`update` 照常。

| 字段 | 说明 |
|---|---|
| `library` | `ok` / `starting` / `error` |
| `list_waveforms` | `enabled`（生效的 `pregenerate_list_waveforms`）、`paused`、`rate`（生效的 `list_waveform_rate`，文件/秒）、`current_rate`（最近 60 秒实测，文件/秒，一位小数）、`job`（当前或最近一次缓存任务，同 `GET /api/v1/waveforms/status` 的 `job`：`done`/`total`/`generated`/`skipped`/`failed`…，没有为 `null`）、`pending`（进行中任务的 `total - done`，否则 `null`）、`failed`（当前文件版本解码失败的记录数）、`queued_on_demand`、`cache_entries`、`cache_bytes`、`cache_limit_bytes`（`list_waveform_cache_mb`）、`over_budget`、`last_error`。与 `GET /api/v1/waveforms/status` 不同，这里不计算 `total` / `remaining`（那两个需要查询数据库） |
| `slots` | `streams` / `previews` / `transcodes` / `waveforms`：各为 `{active, max}`，`max` 即 `max_streams` / `max_previews` / `max_transcodes` / `max_waveforms`（`config.toml`，需重启生效） |
| `waveform_cache` | `{estimated_bytes, limit_bytes}`：完整波形缓存。`estimated_bytes` 是最近一次定期清理（每若干次写入）测得的大小加上之后写入的字节数，为估算值；启动后首次清理前为 `null`。`limit_bytes` 即 `waveform_cache_mb`（需重启） |
| `snapshot` | `library_id`、`revision`（内存中最近已知的值，不读数据库）、`preparing` / `preparing_zstd`（是否正在生成快照）、`cached`（复用的快照副本 `{revision, schema_version, bytes, zstd_bytes}`，没有为 `null`） |
| `roots` | `[{name, online, state: checking/online/waking/offline, message, checked_at}]`，根目录监控的缓存结果；`checked_at` 为最近一次检查或流读取结果的 Unix 秒 |
| `update` | 最近一次更新检查：`state`、`channel`、`current_version`、`selected`（检查选中的发布 tag，没有为 `null`）、`skip_reason`（未提供更新的原因：`disabled` 未启用 / `up_to_date` 已是最新 / `error` 检查失败，否则 `null`）、`message`、`checked_at` |

示例（测试夹具）：

```json
{
  "library": "ok",
  "list_waveforms": { "enabled": false, "paused": false, "rate": 10, "current_rate": 0.0, "job": null, "pending": null,
    "failed": 0, "queued_on_demand": 0, "cache_entries": 0, "cache_bytes": 0, "cache_limit_bytes": 671088640,
    "over_budget": false, "last_error": null },
  "slots": { "streams": { "active": 0, "max": 3 }, "previews": { "active": 0, "max": 5 },
    "transcodes": { "active": 0, "max": 2 }, "waveforms": { "active": 0, "max": 4 } },
  "waveform_cache": { "estimated_bytes": null, "limit_bytes": 1073741824 },
  "snapshot": { "library_id": "86574ab3-929c-4510-92b9-69fb2295ad07", "revision": 1, "preparing": false,
    "preparing_zstd": false, "cached": null },
  "roots": [ { "name": "SFX_Main", "online": true, "state": "online", "message": null, "checked_at": 1792695833 } ],
  "update": { "state": "disabled", "channel": null, "current_version": "0.6.4", "selected": null,
    "skip_reason": "disabled", "message": "此进程未启用自动更新", "checked_at": null }
}
```

管理页「列表波形与资源限制」可修改 `pregenerate_list_waveforms` 和 `list_waveform_rate`（经 `PUT /admin/api/settings`，立即生效）；建议速率 40 文件/秒或更低以保持较低资源占用。其他限制（`max_*`、`*_cache_mb`）只读显示并标注「需重启」，只能在 `config.toml` 修改。

### 2.4 外网反代要求

给配置公网反向代理（如 `https://audiolib.example.com:8443`）的人：

1. **转发目标**：`http://192.0.2.10:8790`（反代与服务端在同一台 Mac 上时也可用 `http://127.0.0.1:8790`；同机反代的连接按 §2.2 同机规则始终算外部请求，即使漏发转发头也需要 token）。整个站点（或路径前缀下的全部路径）转发；路径前缀要在转发时去掉（`/audiolib/api/v1/...` → `/api/v1/...`）。**不要转发 `/admin`**（转发了也只会得到 403，但最好在反代上直接拦掉）。
2. **必须设置 `X-Forwarded-For`**（追加客户端地址）和 **`X-Forwarded-Proto: https`**。服务端靠这些头识别外部请求并要求 token（§2.2）；反代不发这些头时，来自同机/局域网的反代连接会被当成局域网直连而**不需要 token**。配置完成后务必验证：从外网（或带上 `--resolve` 的本机 curl）请求 `<公网地址>/api/v1/library` **不带 token 必须得到 401**，带有效 token 得到 200。
3. **原样透传**请求头 `Range`、`If-None-Match`、`If-Range`、`Authorization`、`Accept-Encoding`，以及响应头 `ETag`、`Content-Range`、`Accept-Ranges`、`Content-Length`、`Content-Encoding`、`Vary`、`Retry-After`、`WWW-Authenticate`、`Content-Disposition` 和所有 `X-*` 头（`X-Preview-*`、`X-Start-*`、`X-Total-Frames`、`X-Duration` 等）。不要改写或去掉 `ETag`（弱化、加引号等都会破坏 `If-None-Match`/`If-Range`）。
4. **关闭缓冲**：`/audio/{id}/stream`、`/audio/{id}/preview`、`/library/snapshot` 是长时间的流式响应（`flac` 预览为 chunked），反代必须边收边发（nginx `proxy_buffering off;`），不能先缓存整个响应；请求体很小，`proxy_request_buffering` 无所谓。
5. **长超时**：读超时至少 1 小时（`proxy_read_timeout 3600s; proxy_send_timeout 3600s;`）——慢速 NAS 唤醒时首字节可能要等 90 秒以上，大文件下载持续很久。
6. **不要再压缩**：关掉反代的 gzip/br 压缩（服务端已对快照和波形使用 `Content-Encoding: zstd`，音频本身不可压缩）；保留服务端的 `Content-Encoding` 和 `Vary: Accept-Encoding`，不要解压或改写。
7. HTTP/1.1 到上游（nginx `proxy_http_version 1.1;`），保持 `Host` 或设置为上游地址均可（有效 token 请求可使用代理 Host；免 token 请求仍受 §2.2 Host 白名单约束）。
8. TLS 证书必须由公共 CA 签发并覆盖所用域名（客户端本版本只信任公共 CA）。

nginx 示例：

```nginx
server {
    listen 8443 ssl;
    server_name audiolib.example.com;
    ssl_certificate     /path/fullchain.pem;
    ssl_certificate_key /path/privkey.pem;

    location /admin { return 403; }
    location / {
        proxy_pass http://192.0.2.10:8790;
        proxy_http_version 1.1;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_request_buffering off;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        gzip off;
        # Range / If-None-Match / If-Range / Authorization 默认原样转发，不要用 proxy_set_header 清掉。
    }
}
```

Caddy 示例（Caddy 默认就会设置 `X-Forwarded-For`/`X-Forwarded-Proto`、透传 Range 等头，且不压缩）：

```caddy
audiolib.example.com:8443 {
    respond /admin* 403
    reverse_proxy 192.0.2.10:8790 {
        flush_interval -1
        transport http {
            read_timeout 1h
            write_timeout 1h
        }
    }
}
```

带路径前缀时（`https://example.com/audiolib`）：nginx 用 `location /audiolib/ { proxy_pass http://192.0.2.10:8790/; … }`，Caddy 用 `handle_path /audiolib/* { reverse_proxy … }`。

## 3. 标识与路径

- **文件 ID**：用服务端数据库的 `audio_files.id`（i64），与 `DataProvider` 签名一致。ID 只在同一个库内有效。
- **id 稳定性**（已核对 `audiolib-db/src/lib.rs`）：重扫时已有文件按 `path_key` 原地 UPDATE，id 不变；扫不到的只标 `missing=1`，不删记录。移动/改名后重扫会生成新 id、旧记录变 missing，需 relink 接回。只有 `remove_audio` / `remove_folder`（服务器模式下即 `POST /audio/remove`、`DELETE /folders/{id}`，§6.8）真正删除记录；被移除的文件重扫后以**新 id** 回到库中。客户端可以缓存 id，遇到 404 或 `missing` 再刷新。
- **库身份**：`GET /library` 返回 `library_id`、`schema_version` 和 `revision`。客户端每次连接比对 `library_id`，变了就清空选择、草稿和缓存（对应现有的 library generation 机制）。
  - `library_id` 保存在服务端自有的旁路文件 `<db>.server-state.json`（与库文件同目录，原子写入），在服务端**第一次打开该 db 路径**时生成（UUID v4）。schema v3/v4 本身没有库身份字段。
  - 原地还原（restore）保留 `library_id`，`revision` 递增；把库复制到另一路径再提供服务会得到新的 `library_id`。旁路文件损坏时服务端拒绝启动，不会悄悄换新 id。
  - 以后可另行设计在库内保存身份（例如元数据表或 SQLite `application_id` + UUID），届时旁路文件只作迁移来源。
- **路径**：服务端**暴露绝对路径**——`GET /roots` 的 `server_path` 以及 `AudioFile.file_path` / `Folder.path` 均为服务端（Mac）上的原始绝对路径。客户端把库里的路径前缀 `server_path` 替换为该根的 WebDAV 地址来访问音频（见 §7）。目前只在局域网使用，因此接受路径暴露；以后若开放到局域网之外再重新评估。例外：`GET /library` 的 `database` 只返回库文件名（如 `library.db`），不含服务端目录；库文件的完整路径不通过 API 暴露。
- **扫描根**：服务端配置允许的根（`[[roots]]`，对应 `allowed_scan_roots`），客户端只能从 `GET /roots` 里选，不能传任意路径。

### 3.1 历史：快照 schema 3 → 5

以下记录已发布 v5 的演进与兼容行为。当前源码为 schema 7 / `ale-s7-cld1-t3-m2-p1`，读取边界为 3..=7，并核对全部 feature；v6 的备份、分批断点恢复、授权退役及部署顺序以[升级契约](Schema_V6_Iteration.md)和[兼容矩阵](Library_Migration_Compatibility.md)为准，v7 的派生专辑和封面边界见[封面契约](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Artwork.md)。下列旧索引、单事务升级与旧客户端写入描述不覆盖 v6/v7 的实际格式准入。

- `SCHEMA_VERSION` = **5**（`crates/audiolib-db/src/schema.rs`，由 `audiolib_db::SCHEMA_VERSION` 导出）。`MIN_SNAPSHOT_SCHEMA_VERSION` = **3**（同文件，客户端的 `MIN_SNAPSHOT_SCHEMA` 直接引用它，不重复定义）。`GET /library` 的 `schema_version` 与快照响应头 `X-Schema-Version` 现在都是 `5`。
- v4 **只新增索引**：不改任何表、列、触发器、FTS，不改行数据，所以 v3 与 v4 用同一套查询；`/library` 的 `stats` 条件由 `COALESCE(missing,0)=0` 改为 `missing=0`（`missing` 列是 `NOT NULL DEFAULT 0`），可命中部分索引。
- **升级行为**：服务端以可写方式打开一个 v3 库时，在一个事务里执行 `PERFORMANCE_INDEXES`（全部 `CREATE INDEX IF NOT EXISTS`，重复或中断后重跑无害）并把 `user_version` 设为 4。首次升级要为全部行建索引，大库需要几十秒到数分钟（实测见 `~/AudioLibScaleTest/report.md`），期间服务端处于启动中状态；库文件会变大。v1/v2 库照旧先补管理表再建全部索引。降级（用旧服务端打开 v4 库）不被支持：旧服务端要求 `user_version` 精确为 3。
- **v3 快照兼容**：客户端接受 `user_version` 为 3、4 或 5 的快照；原快照只读，必要的派生状态升级只在一致性备份所得 owned 副本执行（`open_read_only`、`apply_snapshot_records`、`validate_snapshot` 统一用 `snapshot_schema_supported`），其他版本拒绝。连着仍是 v3 的服务端时，客户端读到的快照没有新索引，只是少了加速，功能一致。
- **新增索引清单（13 个，全部在 `PERFORMANCE_INDEXES`）**：

  | 索引 | 定义 | 用途 |
  | --- | --- | --- |
  | `idx_audio_visible_stats` | `audio_files(folder_id,duration,file_size) WHERE missing=0` | `/library` 统计与文件夹聚合（覆盖索引） |
  | `idx_play_history_audio`（旧 v5） | `play_history(audio_file_id)` | v6 随退役表删除；本机历史不使用主库索引 |
  | `idx_playlist_items_audio` | `playlist_items(audio_file_id)` | 同上 |
  | `idx_project_references_audio` | `project_references(audio_file_id)` | 同上 |
  | `idx_waveform_cache_audio` | `waveform_cache(audio_file_id)` | 同上 |
  | `idx_audio_visible_modified` | `audio_files(modified_at,id) WHERE missing=0` | 按修改时间升序 |
  | `idx_audio_visible_modified_desc` | `audio_files(modified_at DESC,id) WHERE missing=0` | 按修改时间降序（排序为 `value DESC, id ASC`，升序索引会留下临时排序，故单独建降序索引） |
  | `idx_audio_visible_format` | `audio_files(bit_depth,channels) WHERE missing=0` | 位深/声道筛选 |
  | `idx_audio_visible_format_name` | `audio_files(bit_depth,filename COLLATE NOCASE,id) WHERE missing=0` | 位深筛选 + 文件名排序（避免临时排序） |
  | `idx_audio_visible_tagged_any` | `audio_files(filename COLLATE NOCASE,id,user_tags,source_tags_text) WHERE missing=0 AND (COALESCE(trim(user_tags),'')<>'' OR source_tags_text<>'')` | `tagged_only` / `tag_contains` ????????WHERE ?????????v5 ??? `idx_audio_visible_tagged`? |
  | `idx_audio_visible_color` | `audio_files(user_color COLLATE NOCASE,id) WHERE missing=0 AND user_color IS NOT NULL` | 颜色筛选 |

- **Schema v5 (ratings removed from the product).** v5 changes indexes plus one additive derived column (see *Derived column* below): it drops `idx_audio_visible_rating`, `idx_audio_visible_rating_desc` and `idx_user_rating`, and rebuilds `idx_audio_visible_favorite` as `audio_files(filename COLLATE NOCASE,id) WHERE missing=0 AND favorite=1` (no `user_rating`). The `user_rating` column and every stored rating stay untouched but are no longer read, written, filtered or sorted by any client or server code; `MIN_SNAPSHOT_SCHEMA_VERSION` stays **3** and the accepted snapshot range is **3..=5** (client SELECTs never reference `user_rating`). v3/v4 -> v5 runs in one transaction with idempotent statements; v3 libraries also get the v4 indexes. The API no longer has a `rating` field: `AudioFile`, `MetadataPatch`, the `rating_min` filter and the `rating` sort are gone; a legacy `rating` key in a PATCH/batch body is ignored (unknown keys are not rejected) and a legacy `rating` in a response is ignored by the client. Annotation import (CSV/JSONL) ignores a legacy `rating`; exports no longer write it.
- **Derived column `audio_files.source_tags_text` (v5).** `TEXT NOT NULL DEFAULT ''`: the extracted tags (`audiolib_core::source_tags`) joined with `', '` and casefolded (`''` when none). It is never user-visible, never written to `user_tags`, and is maintained by the scanner UPSERT (`metadata_json` and the column are written in the same statement; the FTS text still comes from the same `metadata_json`). `tagged_only` = `user_tags` non-blank OR `source_tags_text<>''`; `tag_contains` = casefolded `user_tags` contains OR `source_tags_text` contains; neither parses `metadata_json` any more. Upgrade: `ALTER TABLE ADD COLUMN` (idempotent, also applied to a v5 library that predates the column) + index, then `open` backfills in id-ordered batches of 20 000 rows, one short transaction per batch, progress in `library_meta.source_tags_progress`, completion marker `library_meta.source_tags_rev = '2'` (re-entrant after a crash; no-op when marked). New libraries and full search rebuilds (`rebuild_search`, legacy import) populate the column and the marker directly. `SqliteProvider::ensure_source_tags_current()` re-runs the backfill explicitly. Until the marker is set (unfinished backfill) and for v3/v4 snapshots (no column), the client falls back to the JSON path (`audiolib_has_source_tags` / `audiolib_source_tags`), so results are identical, only slower. v5 snapshots carry the column and `idx_audio_visible_tagged_any`; client SELECTs never reference the column, so older v3/v4-era clients read v5 snapshots unchanged. Server operators: opening a v3/v4 library with the new binary adds the column and index, but the backfill is **not** quick on a real library. An anonymized large-library measurement (record count and capacity omitted): the tag backfill takes about 3 minutes (198 s) and the FTS rebuild (`ensure_source_search_current`) about 8.5 minutes (513 s); the earlier ~6 s figure came from a synthetic library with tiny `metadata_json` and does not apply. Run the upgrade by hand in a maintenance window: `AudioLibCli db rebuild-search` (backfill, then FTS rebuild, with progress on stderr and per-step timings in the JSON result; `--if-needed` skips an up-to-date library). Until it finishes, searches fall back to the slower JSON path. **Disk:** the FTS rebuild goes through the maintenance path, which leaves an `audiolib-rollback-*.db` in the library directory as large as the library itself; keep that much free space and delete the file after verifying the result. First-snapshot generation for such a library takes ~125 s on the server, which clients cover by retrying BUSY for up to 240 s (see `Rust_Remote_Mode.md`).
- **Legacy-client compatibility (server, v5).** The server accepts legacy rating request fields. Pre-v5 clients that require a `rating` response field must still be upgraded; request acceptance alone does not guarantee their response decoding. (1) `PATCH /audio/{id}/metadata` whose body carries `rating` and no non-null `tags`/`color`/`notes`/`favorite` is a **200 no-op**: it returns the current record (no `rating` key) with `revision_before == revision` (no write, no revision bump, `modified_at` untouched); a `rating` next to real fields is ignored and the real fields are applied as usual. Unknown ids stay 404, auth stays 401, wrongly typed fields stay 400, and a body without `rating` and without any field is unchanged. (2) `POST /audio/metadata/batch` whose `patch` is rating-only is a **200 no-op** (`report.updated = 0`, current `files`, `revision_before == revision`); `ids` are still validated (400 for empty/non-positive/over 10000) and unknown ids are still 404. (3) The server has no list/search endpoint (search runs on the client's snapshot), so `sort=rating` and `rating_min` query parameters are simply ignored by every route (unknown query keys are not rejected) and never produce 400; with `sort=rating` the client-side default order applies. These behaviours are implemented in the server route layer only and are covered by `legacy_*` tests in `server/audiolib-server/tests/api.rs`. New clients must not send `rating`.
- **Extracted tags.** `tagged_only` and `tag_contains` also match the tags derived from `metadata_json` (`audiolib_core::source_tags`, never stored in `user_tags`), and the derived FTS text (`bwf_description_tokenized`) includes the `keywords/category/subcategory/vendor_category/cat_id` fields. A library built before this change must recompute that text once: `SqliteProvider::ensure_source_search_current()` does it (marker `library_meta.source_search_rev = '2'`, same transaction as the rebuild; rollback backup + integrity check via the maintenance path; `Busy` while a scan runs). `open` never calls it, so a server operator runs it by hand in a maintenance window with `AudioLibCli db rebuild-search` (it runs `ensure_source_tags_current()` and then `ensure_source_search_current()`, prints progress to stderr and reports `tags_seconds`, `rebuild_seconds`, `total_seconds`, `rollback_copies`; `--if-needed` is a no-op on a current library, without it a current library is rebuilt as a repair). An anonymized large-library measurement: backfill about 3 minutes (198 s), FTS rebuild about 8.5 minutes (513 s); the rebuild leaves a library-sized `audiolib-rollback-*.db` in the library directory.
- **Include / exclude tag terms (additive, no schema change).** `SearchFilters` gained `include_tags` and `exclude_tags` (string arrays, `#[serde(default, skip_serializing_if = "Vec::is_empty")]`): a record must contain `tag_contains` and every `include_tags` term, and must contain none of the `exclude_tags` terms, each matched exactly like `tag_contains` (case-insensitive literal substring of `user_tags` or the extracted tags). A term on both sides therefore matches nothing. Terms must be non-blank, at most 500 characters and without NUL, at most 64 terms in total (`Validation` otherwise). The GUI and `AudioLibCli search --tag … --exclude-tag …` send the first include term as `tag_contains`, so a single-tag filter is byte-identical to the earlier request. Search runs only on the client's snapshot (there is no server search endpoint), so the terms need no server support: older and newer servers serve the same snapshot, and a request without the new fields deserializes to empty arrays. A lone `tag_contains` keeps its original SQL; with further terms all of them go to one `audiolib_tag_terms(user_tags, folded source tags, JSON terms)` call per row, so the tags are folded once rather than once per term. An exclude-only filter on present files (no query and no other filter, with current derived tag columns) is counted as the present files minus the tagged files containing a term, both read from narrow covering indexes (only tagged files evaluate the terms), and its directory facets subtract the same way per folder; a library-wide duration page over dense matches reads `idx_duration` in order and stops after the page, with or without ANALYZE statistics. With a query and no folder scope, the ranking skips the non-present and excluded tagged files instead of intersecting with every passing file, and the query's facets drop the same excluded files. Results, totals, page order and facets equal the full-scan statement's. Other exclude-only shapes still visit every visible row (files with no tags at all skip the call), while any include term keeps the tagged partial indexes, including the `idx_audio_visible_tagged_folders` directory facet scan.

  `schema_v5` 测试（`crates/audiolib-db/tests/schema_v5.rs`）用 `EXPLAIN QUERY PLAN` 固定上述索引被命中且无临时排序。

## 4. 服务器模式同步模型（已在群里约定）

- 客户端在本机保存库缓存：`%LOCALAPPDATA%/AudioLibExplorer/v1/remote/<library_id>/library.db`。搜索、浏览、统计直接走本地缓存，延迟与本地模式一致。
- 客户端轮询 `GET /library`；`revision` 变化时重新下载 `GET /library/snapshot`，校验后替换本地缓存（先下载到临时文件，校验 `X-Library-Id`、快照 `user_version` 的兼容范围和 SQLite 完整性，读取 `X-Schema-Version` 响应头，再原子替换）。
- `revision` 是保守的变更提示：服务端用专用只读连接轮询 `PRAGMA data_version`（其他连接/进程的每次提交都会改变它，包括只写进 WAL、尚未 checkpoint 的提交），并比对**主库文件**的大小/mtime（默认每 3 秒一次，每次 `/library` 和快照请求也会检查）。WAL 文件的大小/mtime 不参与运行期比对（它们在每次打开/关闭时都会变化，以前会导致每次重启都递增）。服务端停机期间的改动在下次启动时递增一次：主库文件变化，或 WAL 签名（WAL 文件长度 + 32 字节文件头中的 checkpoint 序号和 salt，不含 mtime）变化。服务端退出时故意不关闭自己的 SQLite 连接，避免最后一个连接关闭时把 WAL checkpoint 进主库、下次启动被误判为变更；因此**单纯重启服务端不会递增 `revision`**；**播放计数也不会递增 `revision`**（见下文）。它**可能在没有实际变更时递增**（如其他进程做 WAL checkpoint），但**不会漏掉已提交的变更**。`revision` 先于数据读取，所以返回的数据不会比所标的 `revision` 旧。
- Metadata 编辑：先写服务端，成功后再更新本地缓存。写入端点（§6.3）返回写入后的完整记录以及 `revision_before` / `revision`：服务端在同一个临界区里（所有服务端写入串行）先把此前所有未计入的提交计入 `revision`，记下 `revision_before`，执行写入，然后把 `revision` 设为 `revision_before + 1` 并更新轮询基线，所以这次写入**只递增一次**，轮询不会再为它递增。
  - **客户端规则**：若本地缓存的 `revision == revision_before`，把返回的记录（`file` 或 `files`）写入本地缓存，并把本地 `revision` 设为返回的 `revision`；否则（期间有别的改动）按正常流程下载完整快照同步。写入失败（4xx/5xx）时 `revision` 不变，本地缓存也不要改。
  - 播放（`POST /audio/{id}/plays`，**已弃用**）：空操作，什么都不写，返回 `revision == revision_before`。新版客户端播放不再发送该请求，服务端历史列表返回 `[]`。本开发分支在客户端独立 history.db 记录本机播放事件，主库不再记录播放次数；本地 v5 旧历史在验证备份后导入，服务端和快照不导入，详见 [v6 契约](Schema_V6_Iteration.md)。
  - 已知窄窗口：服务端写入执行期间若恰好有其他进程提交，那次提交会并入同一次递增；客户端按上面的规则只应用返回的记录，那次外部改动要等下一次变更（`revision` 再变）才会同步到。
  - 写入端点受 30 秒请求超时约束；若超时返回 408，写入可能已经提交，`revision` 会照常变化，客户端下一次轮询会走快照同步。
- 远程扫描（§6.6）：扫描期间 `revision` 冻结，结束时有改动才递增一次（无改动的增量扫描不递增）；客户端照常按 `revision` 变化同步。
- 移除记录/文件夹（§6.8）：与 metadata 写入同一个临界区，返回 `revision_before` / `revision`；真正删除了记录（或文件夹）时只递增一次，什么都没删时不递增（`revision == revision_before`）。客户端规则同上：`revision_before` 等于本地值时可直接在本地缓存里删掉这些记录，否则做快照同步。
- `library_id` 变化：清空本地缓存目录、选择和草稿，重新下载快照。

## 5. 分页

- 搜索输入按客户端快照上的 `SearchRequest` 校验：查询最多 500 个字符；独立的非 Han 字母词至少需要两个可搜索 Unicode grapheme（组合重音不另计）。这是已确认的产品行为。GUI 输入一个字母时取消旧请求、保留当前结果与目录并显示“字母搜索请至少输入 2 个字符”，不发送新搜索。直接调用 core/db 或 CLI 则返回 `Validation`，不会当作零命中或内部数据库错误。多词中的单字母代码（例如 `P-51`）、单个 Han 字和纯数字沿用原规则。
- 两个字母以上支持词内连续片段：`expl`、`plos`、`sion` 匹配 `explosion`，多个词之间为 AND；用户文本中的标点、`OR` 等不作为 FTS5 运算符。当前服务端 **尚未实现**下表的 `POST /search` 与 `/search/folders`，正常服务器模式在本地快照查询，因此不存在当前 HTTP 搜索端点的单字母 400/500 响应；若未来开放端点，按 §2.1 的现有映射应为 HTTP 400 `VALIDATION`。
- v1 沿用 `SearchRequest` 的 `limit` + `offset`（UI 现在每页 200），响应带 `total_count`。理由：与本地实现完全一致，remote 改动最小。
- 游标分页（服务端提议）列为待定：如果服务端需要快照一致性，建议游标为不透明字符串 `next_cursor`，内部编码排序键 + 最后一个 id；`SearchRequest` 再加可选 `cursor` 字段，有游标时忽略 `offset`。
- 扫描历史与客户端本机播放历史使用 `limit` + `offset`；服务端兼容 `/history/plays` 始终返回 `[]`。

## 6. 端点清单

状态：✅ = 已实现；⏳ = 计划中。

| 状态 | 方法 | 路径 | 对应 `DataProvider` | 请求 | 响应 |
|---|---|---|---|---|---|
| ✅ | GET | `/health` | — | 免鉴权 | `{ "status": "ok" \| "starting" \| "error", "version": "0.6.0-dev.1", "api_version": "v1", "schema_version": 6, "commit": "abc1234" \| "unknown", "message"?: "…", "public_url": "…" \| null }`，见 §6.2 |
| ✅ | GET | `/library` | `library_info` | — | `LibraryInfo` 字段（含 `database`（仅文件名，如 `library.db`，不暴露服务端完整路径）、`schema_version`、`stats`、`database_bytes`、`wal_bytes`）+ `library_id`、`revision`；`stats` 按 revision 缓存（内存 + 库旁 `<库文件>.stats.json`，重启后立即可用）：revision 变化后若上次计算耗时 >250 ms，先返回上一份统计并在后台重算（计数只可能滞后最近几次写入），否则同步重算；播放记录与元数据修改不影响这些统计，写入后缓存直接沿用到新 revision（不整表重算）；启动时后台预热（库旁缓存已是当前 revision 时跳过）；`database_bytes` = 主库文件 + `-wal` 文件的字节数（库实际占用），`wal_bytes` 为其中 WAL 部分 |
| ✅ | GET | `/library/snapshot` | `backup_library`（只读导出） | — | SQLite 快照字节，见 §6.1 |
| ✅ | GET | `/roots` | `folders`（求 `file_count`）+ 根状态缓存 + 扫描历史 | — | `[{ "name", "server_path", "webdav_url": "…" \| null（功能未完善，见 §7）, "file_count": n \| null, "missing_count": n, "online": bool, "status": "checking" \| "online" \| "waking" \| "offline", "status_message": "…" \| null, "checked_at": unix秒 \| null, "last_scan": { … } \| null, "scanning": bool }]`，见 §6.4 |
| ✅ | POST | `/roots/{name}/check` | 根状态 | 无请求体 | 立即重查该根（最多约 2 秒），返回该根的 `/roots` 条目；未知根名 404 `NOT_FOUND`，见 §6.4 |
| ⏳ | GET | `/stats` | `stats` | — | `LibraryStats` |
| ⏳ | POST | `/search` | `search` | `SearchRequest` | `SearchPage` |
| ⏳ | POST | `/search/folders` | `search_folders` | `SearchRequest` | `Folder[]` |
| ⏳ | GET | `/folders?parent_id=<id>` | `folders` | 不带 `parent_id` 为根 | `Folder[]` |
| ✅ | GET | `/audio/{id}` | `get_audio` | — | `AudioFile`；无此记录 404 `NOT_FOUND`，非整数 id 400 `VALIDATION` |
| ✅ | PATCH | `/audio/{id}/metadata` | `update_metadata` | `MetadataPatch` | `{ "file": AudioFile, "revision_before": n, "revision": n+1 }`，见 §6.3 |
| ✅ | POST | `/audio/metadata/batch` | `update_metadata_batch` | `BatchMetadataRequest`（`tag_mode` 可省略，默认 `replace`） | `{ "report": BatchMetadataReport, "files": AudioFile[], "revision_before": n, "revision": n+1 }`，见 §6.3 |
| ✅ | POST | `/audio/remove` | `remove_audio` | `{ "ids": [n, …] }`（1..=10000 个） | 200 `{ "removed": n, "not_found": [id…], "revision_before": n, "revision": n \| n+1 }`；只删库记录，不删文件；受 `manage_mode` 限制，扫描中 409 `BUSY`，见 §6.8 |
| ✅ | DELETE | `/folders/{id}` | `remove_folder` | 无请求体 | 200 `{ "removed": n, "folders_removed": n, "revision_before": n, "revision": n+1 }`（整个子树）；未知 404，库根顶层文件夹 400，见 §6.8 |
| ✅ | GET | `/capabilities` | — | — | `{ "manage_mode": "all" \| "lan" \| "off", "external": bool, "can_manage": bool, "root_management": true }`；需鉴权，库启动中也可用，见 §6.9；另有 `list_waveforms`、`pregenerate_list_waveforms`（§9.2） |
| ✅ | GET | `/fs` | 服务端目录（有界只读浏览） | `path?`、`hidden?` | 起点或目录列表；受 `manage_mode` 限制，见 §6.10 |
| ✅ | POST | `/roots` | 持久化根目录设置 | `{ "name", "path" }` | 201 根目录条目；受 `manage_mode` 限制，见 §6.10 |
| ✅ | PATCH | `/roots/{name}` | 原子重定位根目录记录 + 持久化设置 | `{ "path" }` | 200 根目录条目；保留 ID、Metadata 与标注，见 §6.10 |
| ✅ | DELETE | `/roots/{name}` | 数据库记录移除 + 持久化设置 | — | 200 `{ "name", "removed", "folders_removed", "revision_before", "revision" }`；不删除源 WAV，见 §6.10 |
| ⚠️ 已弃用 | POST | `/audio/{id}/plays` | — | 无请求体 | 200 `{ "file": AudioFile, "revision_before": n, "revision": n }`：**空操作**，不写库、不递增 revision，`file` 为当前记录（id 不存在仍 404）；仅为兼容旧客户端保留，新客户端不要调用，见 §6.3 |
| ⚠️ 已弃用 | GET | `/history/plays?limit=&offset=` | — | `limit` 默认 100（1..=10000），`offset` 默认 0（仍校验） | 总是空数组 `[]`；仅为兼容旧客户端保留，见 §6.7 |
| ✅ | GET | `/history/scans?limit=&offset=` | `scan_history` | 同上 | `ScanSession[]`，新的在前，见 §6.7 |
| ✅ | POST | `/scans` | 扫描（`ScanService::scan`；受 `manage_mode` 限制，§6.9） | `{ "root": "<name>", "subpath": "", "mode": "incremental" \| "full", "workers": null }` | 200 `ScanResponse`（`ScanProgress` 字段 + `root`/`subpath`/`mode`/`active`/`pause_requested`/`cancel_requested`/`revision`，含新的 `session_id`），见 §6.6 |
| ✅ | GET | `/scans/current` | 扫描进度 | — | 200 `{ "scan": ScanResponse \| null }`（没有扫描时为 `null`，不是 404），见 §6.6 |
| ✅ | GET | `/scans/{session_id}` | 扫描进度 | — | `ScanResponse`；未知会话 404 `NOT_FOUND`，见 §6.6 |
| ✅ | POST | `/scans/{session_id}/pause` · `/resume` · `/cancel` | 扫描控制（受 `manage_mode` 限制，§6.9） | 无请求体 | `ScanResponse`；状态不允许时 409 `SCAN_STATE`，见 §6.6 |
| ✅ | GET / HEAD | `/audio/{id}/stream[?download=1]` | 音频流 / 下载 | 见 §8 | 音频字节（`download=1` 时附 `Content-Disposition: attachment`，§8） |
| ✅ | GET / HEAD | `/audio/{id}/waveform?buckets=&colors=&start_frame=&end_frame=` | `audiolib_audio::waveform` | 见 §9 | `WaveformData` JSON + `start_frame`/`end_frame`/`total_frames`（可 zstd），见 §9 |
| ✅ | POST | `/waveforms` | 列表缩略波形（批量，只读，不受 `manage_mode` 限制） | `{ "ids": [n, …]（1..=200）, "buckets": 128, "known"?: { "<id>": "<etag>" } }` | 200 `{ "library_id", "buckets", "retry_after", "items": [{ "id", "status": "ok" \| "not_modified" \| "pending" \| "missing" \| "root_offline" \| "error", "etag"?, "n"?, "peaks"? }] }`（单声道 128 列 min/max，int16 LE base64；可 zstd），见 §9.1 |
| ✅ | GET | `/waveforms/status` | 列表缩略波形预生成 / 按需队列进度 | — | `{ "enabled", "state", "yield_reason", "total", "cached", "remaining", "queued_on_demand", "failed", … }`，见 §9.2 |
| ✅ | POST | `/waveforms/pregeneration/pause` · `/resume` | 手动暂停 / 继续预生成（受 `manage_mode` 限制） | 无请求体 | 同 `/waveforms/status`，见 §9.2 |
| ✅ | GET / PATCH | `/settings` | 客户端可读写的服务端设置（PATCH 受 `manage_mode` 限制） | PATCH `{ "pregenerate_list_waveforms": bool \| null }` | `{ "pregenerate_list_waveforms", "pregenerate_list_waveforms_source", "can_change" }`，见 §9.2 |
| ✅ | GET / HEAD | `/audio/{id}/preview?codec=pcm\|flac&sr=&bits=&ch=[&t=]` | 低带宽试听预览 | 见 §8.1 | `pcm`：进程内生成的 WAV（有长度，可 Range）；`flac`：ffmpeg 实时转码的分块 FLAC 流 |
| ✅ | GET / HEAD | `/audio/{id}/artwork[?side=]` | 内嵌封面（按需读文件，不入库） | `side` 可省略，1..=512 | `image/jpeg` 或 `image/png`；无封面 / 文件缺失 / 提取失败 404，见 §8.2 |

管理页 `/admin` 和 `/admin/api/...` 不在 `/api/v1` 下，只供局域网浏览器使用，见 §2.3。

所有针对库记录或扫描会话的修改请求必须带 `X-AudioLib-Library-Id: <客户端已打开快照的 library_id>`：`PATCH /audio/{id}/metadata`、`POST /audio/metadata/batch`、`POST /audio/{id}/plays`、`POST /audio/remove`、`DELETE /folders/{id}`、`POST /scans` 及其 pause/resume/cancel 控制，以及 `POST /roots`、`PATCH /roots/{name}`、`DELETE /roots/{name}`。服务端校验后固定该请求使用的 AppState（库身份及 DataProvider），直到操作结束；不能只先读取当前库 ID 再向可能切换的提供者写入。缺失返回 409 `LIBRARY_ID_REQUIRED`，不匹配返回 409 `LIBRARY_CHANGED`，均不写入且不递增 revision。`POST /roots/{name}/check` 和 `/admin/api/...` 不要求该客户端头。

`GET /library` 新增 `identity_bound_writes: true`，表示服务端强制上述库身份约束。旧服务器缺少该字段（视为 false）仍可同步、试听，但新版客户端拒绝向它写入；不能用写入前 GET /library 代替原子身份约束。收到上述 409 时客户端停止写入，保留未保存草稿，提示同步并切换到新库后再编辑。旧客户端缺少请求头时写入被拒绝，读取不受影响。

- `/roots.file_count`：该根在库中对应文件夹的非 missing 文件总数（含子目录），只查库、不访问磁盘（根离线时照常返回）；根尚未扫描时为 `null`。`missing_count`、`last_scan`、`scanning` 见 §6.4。`webdav_url` 未配置时为 `null`（**WebDAV 功能未完善**，见 §7）。
- 扫描进度 v1 用轮询（**建议每 500 ms** `GET /scans/{id}`，GUI 本来就会节流合并进度）；以后需要再加 SSE `GET /scans/{id}/events`。同一个库同时只允许一个扫描，冲突返回 409 `BUSY`。详见 §6.6。

### 6.1 `GET /library/snapshot`

- 服务端用现有 `backup_library`（SQLite backup API，单个读事务内的一致快照）写入 `snapshot_dir` 下的临时目录，再以 `application/octet-stream` 流式返回。
- **按 revision 准备**：未指定 `snapshot_id` 时选择最新 `(library_id, revision)`，revision 未变复用准备结果，准备过程单飞。已发送的固定表示另受下述续传保留规则保护，不会因为播放或 Metadata 写入推进 revision 就失效。
- **压缩（可选）**：请求带 `Accept-Encoding: zstd`（`q=0` 表示不接受）时返回 zstd 压缩（level 3，多线程）的同一个 SQLite 文件，`Content-Encoding: zstd`；压缩版也按 revision 缓存，第一次请求时生成。不带时照旧返回原始文件。
- 响应头（200）：

  | 头 | 说明 |
  |---|---|
  | `Content-Length` | 实际发送的字节数（压缩时为**压缩后**长度） |
  | `X-Uncompressed-Length` | 未压缩 SQLite 文件的字节数（不压缩时等于 `Content-Length`） |
  | `Content-Encoding` | 仅压缩时为 `zstd` |
  | `ETag` | 精确不可变表示的强 ETag `"snapshot-<UUID>"`；重新生成或切换编码时不同 |
  | `X-Snapshot-Id` | 固定传输表示的 UUID，客户端应作为不透明标识保存 |
  | `Accept-Ranges` | `bytes`；续传偏移是实际传输表示的字节位置 |
  | `Content-Range` | 206 时为 `bytes <start>-<end>/<whole-length>`；416 时为 `bytes */<whole-length>` |
  | `X-Library-Id` / `X-Library-Revision` | 快照所属的库和 **生成它所依据的 revision**（已有头，名称不变）；快照内容不旧于该 revision |
  | `X-Schema-Version` | 快照文件自身的 `user_version`（当前为 `6`；边界 `3..=6`，还须通过完整格式及 capability 协商，见 §3.1） |
  | `Vary` | 总是 `accept-encoding`（含 304 和错误响应） |
  | `Content-Disposition` | `attachment; filename="audiolib-<library_id>-r<revision>.db"` |
  | `Cache-Control` | `no-store` |

- `GET /library/snapshot?snapshot_id=<UUID>` 只访问该固定表示（`snapshot_id` 须为规范小写带连字符的 UUID，否则返回 400 `VALIDATION`），不准备或替换成最新快照；仍须鉴权，且对象必须属于当前库。支持单区间 `Range` 和强 `If-Range`：命中返回 206，不匹配返回该对象完整 200，不可满足的范围返回 416。zstd 范围针对压缩字节，不能将后缀单独解压。对象编码不被接受返回 406 `SNAPSHOT_ENCODING_NOT_ACCEPTABLE`，不存在或失效返回 410 `SNAPSHOT_EXPIRED`。
- `If-None-Match` 与所选不可变表示的 ETag 匹配时返回 **304**；不能仅凭相同 revision 推定重新生成的字节相同。304 同样带 `ETag`、`X-Snapshot-Id`、`Accept-Ranges`、`X-Uncompressed-Length`、`X-Library-Id` / `X-Library-Revision`、`X-Schema-Version` 及兼容性协商头，无响应体。
- **ETag 格式变更**：旧版本为弱 ETag `W/"<library_id>-r<revision>"`，现为强 ETag `"snapshot-<UUID>"`。旧客户端保存的旧 ETag 不再匹配，只会多完整下载一次，之后照常。
- **续传保留上限**：进程内最多 4 个表示、合计 64 GiB、空闲最多 24 小时；容量压力按 LRU 提前淘汰无活动响应的对象。活动响应持租约，不被淘汰。仅在单个表示超过 64 GiB，或活动传输（不可淘汰）的表示合计加上新对象超过 64 GiB 预算时返回 507 `INSUFFICIENT_STORAGE`；因并发上限 2 小于保留上限 4，实际不会因条目数满而返回 507。仅保留实际传输表示，原始备份与压缩暂存另占磁盘空间。服务端重启会使旧 ID 失效；客户端须提示后安全重新下载，绝不能混接两个对象。
- 并发：同时进行中的快照响应（准备 + 发送）最多 2 个，带 `snapshot_id` 的续传请求同样占用这 2 个名额；超出返回 409 `BUSY`，客户端稍后重试。
- **磁盘空间检查**：生成原始快照前按库文件大小（含 WAL）、生成 zstd 副本前按原始大小的 1/8（实测约 6–7 %）检查 `snapshot_dir`（默认库文件旁 `snapshots/`）所在卷的剩余空间，另留 max(64 MiB, 2 %) 余量；不足（或磁盘写到一半满了）时，服务端先按 LRU 逐个淘汰无活动响应、属于更早 revision 的已保留快照并重试；正在准备的同一 revision 的表示（可能是其续传副本）从不淘汰，某次淘汰未实际释放磁盘空间（文件仍被其他持有者占用）或已无可淘汰对象时，立即返回 **507 `INSUFFICIENT_STORAGE`**（预检查不足时不写任何字节）。无法取得剩余空间的平台（非 Unix）跳过检查。`backup_library`（桌面端备份）使用同一检查。
- **准备在后台进行**：快照（及 zstd 副本）的准备在独立的后台线程里做，不绑定发起它的请求（每种编码单飞）。请求最多等 `snapshot_wait_secs`（服务端配置，1..=3600，默认 90，常规部署须小于客户端 240 秒的响应头超时）；超时返回 **503 `BUSY` + `Retry-After: 5`**，准备继续，客户端重试时加入同一次准备（或直接拿到已完成的快照），不会再拷贝一份。客户端对 503 `BUSY` 按 `Retry-After` 重试。
- 匿名实际库的历史实测（记录数、容量及存储位置不公开）：首次生成 1.9 秒（旧实现每 256 页停 10 ms，需 11–12 秒），缓存命中首字节 < 1 ms；zstd 压缩 0.19 秒，压缩后为原大小的 6.7%，客户端解压约 0.2 秒。压缩耗时对比：zstd -9 为 0.6 秒、-19 为 21 秒，gzip -6 为 4 秒。准备期间 `/library` 照常（实测 36 ms），写入不被阻塞。磁盘占用见上文续传保留规则：最多保留 4 个已传输表示、合计 64 GiB、空闲 24 小时后过期，活动传输受保护不被淘汰；服务端重启后旧 `snapshot_id` 全部失效，启动时清理 `snapshot_dir` 中遗留的 `audiolib-snapshot-*` 目录（见下条）。这些时间仅描述该次测量，不保证其他库的准备时间。
- 进程崩溃/被杀后遗留的 `audiolib-snapshot-*` 临时目录在服务端下次启动时清理。默认位于库文件旁的 `snapshots/`；只删除带所有权标记且锁已释放、目录至少 30 秒的残留，活动文件和未标记目录保留。

### 6.2 启动状态与 `/health`

可恢复搜索升级期间额外返回 `upgrade`，例如
`{"phase":"indexing","completed":5000,"total":900000}`。阶段包含 `preparing`、
`backing_up`、`tokenizing`、`indexing`、`validating`、`checkpoint` 和 `source_tags`；
行数为该阶段已提交的恢复点，非全流程百分比，不含路径或凭据。就绪后省略此字段。
升级中断后从已提交批次恢复，业务接口在完成前仍为 503。
详见 [升级恢复和 WAL 生命周期](../Rust_Upgrade_Recovery.md)。

服务端**先监听、后开库**：进程启动后立即绑定端口提供服务，库（及 revision 轮询）在后台线程打开。这样外置卷上的库被 macOS 权限弹窗（可移动卷访问授权）卡住时，端口仍然可用，launchd 也不会反复重启进程。

| 阶段 | `GET /health`（始终 HTTP 200） | 需鉴权的端点 |
|---|---|---|
| 正在打开库 | `{"status":"starting","version":"…","public_url":…}` | 需要 token 时（§2.2）先校验 token（错误仍为 401），再返回 503 `UNAVAILABLE` + `Retry-After: 5` |
| 已就绪 | `{"status":"ok","version":"…","public_url":null}`（里程碑 1 的字段 + `public_url`） | 正常 |
| 打开失败 | `{"status":"error","version":"…","message":"The library could not be opened; the server keeps retrying","public_url":…}` | 503 `UNAVAILABLE`，`message` 为同样的固定文字 |

- 版本字段（三种状态都带，只增不改，旧客户端忽略未知字段）：`version` 程序版本（工作区 `Cargo.toml`）；`api_version` API 版本（`"v1"`，对应 `/api/v1/`）；`schema_version` 本服务端构建支持的库 schema 版本（整数，取 `audiolib-db` 的 `SCHEMA_VERSION`，不是库文件当前的 `user_version`）；`commit` 构建时的 git 短哈希，取不到为 `"unknown"`。三套版本的规则见 `doc/Rust_Versioning.md`。
- `public_url`：管理员在管理页（§2.3）或 `config.toml` 配置的公网地址（规范化后，如 `https://audiolib.example.com:8443`，不含 `/api/v1`），未配置时为 `null`；三种状态都带。`/health` 不需要 token。
- 打开失败不会退出进程：错误写入日志，按 5 秒起、每次翻倍、最长 300 秒的间隔重试；之后一旦成功即转为 `ok`（例如用户在弹窗里授权、外置卷挂载后）。`ok` 之后不会再回到 `starting`/`error`。
- `/health` 和 503 `UNAVAILABLE` 的 `message` 都只给固定文字，不含路径或密钥；具体原因只写入服务端日志。
- 客户端：`/health` 为 `starting`/`error` 或收到 503 `UNAVAILABLE` 时，按“服务器暂不可用”处理并稍后重试，不要当成空库或 token 错误。503 `UNAVAILABLE` 只表示库未就绪；库根离线是另一个错误码 `ROOT_OFFLINE`（§2.1、§6.4），此时服务器和库都可用。

#### 历史：v5 派生搜索索引升级与维护窗口

以下是 v5 的原地派生状态升级，不是 v6 执行器。新版会在完整备份后构建 owned 候选库，以持久任务及批次游标支持恢复，并在校验后发布；见 [v6 契约](Schema_V6_Iteration.md)。

schema 仍为 5；`audiolib_schema_features.tokenization` 从旧修订 1/2 升为 3 时会重新计算全部派生搜索文本并重建 FTS5，`search_indexes` 修订 1 另补覆盖索引。不会重扫源 WAV，也不改用户 Metadata、标注和扩展字段。大库首次启动需要维护窗口：先通过受支持的备份流程保存库，再启动新版；按数据与可用磁盘空间预留时间，不以搜索的 **P95 latency <500ms for 1M+ records** 目标推断升级时长。

服务端以默认 `info` 日志输出 `preparing`、`tokenizing`（`completed`/`total`，每 5000 行及最终值）、`building_fts`、`source_tags` 和经过秒数；FTS5 构建阶段没有可信百分比。日志进度不是已提交数据量。

派生文本、FTS5 和相关版本标记在一个事务中提交；取消/失败会回滚，重启从头重建，不支持分批提交或跨进程断点续做。若仍需补齐，`source_tags` 使用独立的每批 20,000 行事务，在同一事务中提交结果和进度；同一提取修订可从已提交进度续做，提取修订变化时重新开始。完整搜索重建已经填充该列及完成标记时，此阶段直接跳过。升级的独占 administration lease 排除参与协议的跨实例写入。HTTP 在后台开库及升级期间仍可访问 `/health`；需要已打开库的业务端点在鉴权通过后返回 `503 UNAVAILABLE` 和 `Retry-After: 5`，就绪后恢复正常。`/capabilities` 和不依赖库的管理功能仍可访问；鉴权失败仍返回 401。升级进度仅写入日志。只读客户端快照只在 owned 副本上升级，原快照保留。可用 `cargo run --release --locked -p audiolib-db --bin upgrade-benchmark -- --source <snapshot.db>` 对源库只读备份到临时副本后测量，Windows 副本结果与 macOS 实测应分别记录，见 [Rust 验证记录](../Rust_Validation.md)。

### 6.3 Metadata 写入（已实现）

两个 Metadata 写入端点及播放兼容端点都按 §2.2 鉴权，库未就绪时 503 `UNAVAILABLE`，受 30 秒超时约束。Metadata 请求体为 JSON，必须声明 `Content-Type: application/json`（可带 `charset`，见 §2），最大 8 MiB（超出 413 `PAYLOAD_TOO_LARGE`）。只改库，永远不写源 WAV。Metadata 成功后 `revision == revision_before + 1`，客户端按 §4 的规则应用返回的记录。播放兼容端点不改变 revision。

`PATCH /audio/{id}/metadata`，请求体 `MetadataPatch`，省略的字段不变（`color` 传 `""` 清除颜色）：

```json
{ "tags": "loud door", "color": "#FF8800", "notes": "slam", "favorite": true }
```

响应 200（`file` 为写入后的完整 `AudioFile`，与 `GET /audio/{id}` 相同）：

```json
{
  "file": {
    "id": 3, "file_path": "/srv/audiolib/music/SFX/door.wav", "filename": "door.wav", "folder_id": 1,
    "file_size": 52, "sample_rate": 48000, "bit_depth": 16, "channels": 1, "duration": 0.0000833,
    "tags": "loud door", "color": "#FF8800", "notes": "slam", "favorite": true,
    "bext_description": "", "attributes": {}, "missing": false
  },
  "revision_before": 41,
  "revision": 42
}
```

`POST /audio/metadata/batch`，请求体 `BatchMetadataRequest`（`ids` 1..=10000 个正整数，`patch` 至少一个字段；`tag_mode` 为 `replace` | `append` | `remove`，省略为 `replace`）。整批在一个事务里，任一 id 不存在则整批回滚、返回 404。扫描进行中返回 409 `BUSY`。

```json
{ "ids": [3, 7, 3], "patch": { "tags": "wood" }, "tag_mode": "append" }
```

响应 200（`report.updated` = 去重后的 id 数；`files` 为这些记录写入后的完整 `AudioFile`，按请求顺序、去重）：

```json
{ "report": { "updated": 2 }, "files": [ { "id": 3, "…": "…" }, { "id": 7, "…": "…" } ], "revision_before": 42, "revision": 43 }
```

`POST /audio/{id}/plays`，无请求体：验证库身份、id 与记录存在性后返回现有 `AudioFile`，
不写数据库、不增加 revision 或 `data_version`。响应 200，外层 revision 响应不变，内部 AudioFile 按下述 v6 字段契约：

```json
{ "file": { "id": 3, "…": "…" }, "revision_before": 43, "revision": 43 }
```

下一版 p1 的实际库修改（Metadata、批量修改、移除、根目录及扫描）须携带
`X-AudioLib-Client-Compatibility`，同时通过两端程序/协议范围及当前完整库格式的读取能力检查。
缺失、无效或只接受旧格式的客户端在写入前收到 409 升级提示，revision/data_version 不变。
鉴权及管理权限优先；库 ID 校验继续独立生效。无写入的两个旧播放路由保留原 headerless 行为。

下一版 v6 的 AudioFile JSON 删除 `play_count`，`attributes` 不再附加 `last_played_at`；CSV 同样不导出计数。
旧服务器响应中的多余字段被新客户端忽略。已发布旧客户端的计数是必填字段，不能将旧播放路由的
HTTP 200 视为 DTO 兼容保证；须先升级客户端，之后在维护窗口升级服务端。两个旧播放路由仍保留
校验和无写入语义。播放历史转为客户端本机独立存储，详见 [v6 契约](Schema_V6_Iteration.md)。

错误：`color` 不是空串或 `#RRGGBB`、`tags` > 16 KiB、`notes` > 1 MiB、非法 JSON / 字段类型不对、非整数 id、空 `ids` 或空 `patch`、未知 `tag_mode` → 400 `VALIDATION`；无此 id → 404 `NOT_FOUND`；需要 token 时（§2.2）token 缺失或错误 → 401。任何错误都不改库，`revision` 不变。

### 6.4 库根目录状态与慢速磁盘（已实现）

库根目录通常在 NAS 上，NAS 硬盘休眠后唤醒要 20–30 秒甚至更久。服务端因此遵守：

1. **只有 `/audio/{id}/stream`（GET/HEAD，含 `download=1`）、`/audio/{id}/preview`（GET/HEAD）、`/audio/{id}/waveform` 和 `/audio/{id}/artwork`（GET/HEAD，§8.2）会访问根目录下的文件**，而且没有任何时间上限：不受 30 秒 408 限制，打开/stat/首次读取等多久都行（≥ 90 秒没问题）；唯一的超时是 10 秒的请求头读取，只针对客户端发送请求头。打开文件在 tokio 的阻塞线程池里进行，不占用异步运行时；客户端在等待期间断开时，处理函数连同它占用的并发名额（`max_streams` / `max_previews` / 波形名额）立即释放，即使阻塞的 `open()` 本身还卡着（例如 macOS 的“网络宗卷”授权弹窗未处理时）。这些卡住的打开会在服务端登记：每个根最多同时存在 4 × (`max_streams` + `max_previews`) 个未结束的打开（流和预览的打开共用这个上限），超出时新请求直接返回 503 `ROOT_OFFLINE`（`…earlier file opens have not finished yet`），所以卡住的线程有上限。打开超过 10 秒时服务端日志记一条 WARN（含根名和文件路径），结束时若总共超过 10 秒再记一条。§9.1–§9.2：`POST /waveforms` 只对文件做 `stat`（有 2 秒预算，不读音频数据，根缓存为 `offline` 时完全不访问）；列表缩略波形的后台工作线程会读取音频数据，同样没有时间上限。
2. **`/health`、`/library`、`/library/snapshot`、`/roots`、`/audio/{id}`、`/history/...`、`/scans/...` 的查询和控制端点以及 metadata 写入端点从不访问根目录**（只读写本机磁盘上的库文件和内存缓存），NAS 休眠或离线时照常快速响应。`POST /scans`（§6.6）是例外：扫描本身就要读根目录，它在独立后台线程里进行，没有时间上限。服务端启动时也不访问根目录；根检查线程在库打开成功之后才启动。
3. **根状态由后台线程检查并缓存**，`/roots` 只读缓存。每个根一个检查线程：启动时、每 `root_check_secs` 秒（默认 300）、以及有请求要求重查时（见下）各检查一次。检查只做根目录的 `realpath` + `stat`（不列目录、不读文件），**不会为保持唤醒而轮询**；若连这个也不想要，把 `root_check_secs` 调大（最大 86400）。进行中的文件打开与进行中的检查同等对待：自上次成功以来超过 5 秒未完成 → `waking`，超过 `root_offline_secs` → `offline`；在此之后的一次成功（检查或打开）立即恢复 `online`。

`/roots` 的状态字段：

| `status` | `online` | 含义 |
|---|---|---|
| `checking` | `true` | 启动后第一次检查还没结束（按在线处理） |
| `online` | `true` | 最近一次检查或音频流打开成功 |
| `waking` | `true` | 之前在线，新的检查或文件打开已超过 5 秒未返回（多半是硬盘在唤醒）；**慢不等于离线** |
| `offline` | `false` | 确定的失败（根目录不存在/卷未挂载、无权限、I/O 或连接错误），或检查/文件打开超过 `root_offline_secs` 秒（默认 120）仍未返回（`Root has not answered for N s`）；`status_message` 给出原因（不含路径） |

- `checked_at`：最近一次完成的检查（或音频流打开）的 Unix 秒，启动后尚未完成时为 `null`。
- 离线后无需人工干预：任何一次检查或音频流打开成功都会立刻恢复 `online`。
- 根缓存为离线时：`/audio/{id}/stream`（**GET 和 HEAD 都一样**，所以客户端可以用一个 `HEAD` 触发重查）、`/audio/{id}/waveform` 和 `/audio/{id}/artwork` 先做一次**有界的即时重查**（最多等约 2 秒，由检查线程执行）：卷已恢复就直接正常响应（不会先收到一次 503）；仍离线或 2 秒内没有结论则返回 503 `ROOT_OFFLINE` + `Retry-After: 5`，`message` 如 `Library root "SFX_Main" is offline: Root directory not found (volume not mounted?)`，未完成的检查在后台继续。重查请求会合并：每个根同一时间只有一个检查在跑，并发请求等同一个结果，不会形成检查风暴。
- **`POST /roots/{name}/check`**：同样的有界即时重查（约 2 秒），返回该根更新后的 `/roots` 条目。2 秒内没有结论时返回当时的状态，检查在后台继续：之前在线的根显示 `waking`，之前离线的根保持 `offline`（直到某次检查成功），首次检查未完成为 `checking`。未知根名返回 404 `NOT_FOUND`。适合客户端“重试”按钮。
- 文件打开失败时，服务端会顺带检查该根：根本身不可达则标记离线并返回 503 `ROOT_OFFLINE`；根可达但文件访问遇到超时/连接断开/网络文件句柄失效（`ETIMEDOUT`、`ENOTCONN`、`ECONNRESET`、`ESTALE` 等）时同样返回 503 `ROOT_OFFLINE`（`Root is not answering reliably: …`，可重试，不改根状态）；根可达且文件不存在才按文件缺失返回 404，其余 I/O 错误（如 `EIO`）为 500 `IO`。**根离线或变慢都不会把库里的文件标为 missing**。
- 扫描器遵守同样的规则：根目录不存在/不可读时扫描直接失败，不改任何记录；扫描途中目录读取出错（例如卷断开）时不做 missing 对账；全量扫描遇到**完全为空**的根目录（只有 `.DS_Store` 等隐藏文件也算空；典型的未挂载挂载点）也不做 missing 对账（代价：用户真的删光了根目录下所有文件和文件夹时，记录不会被标 missing，直到根目录里重新有内容后再全量扫描；这种情况下流端点对这些文件返回 404）；慢只会让扫描变慢，扫描本身没有超时。

`/roots` 的扫描相关字段（只查库，不访问根目录；用 `missing = 1` 部分索引和很小的 `scan_sessions` 表，匿名实际库上实测：局域网 `GET /roots` 整个请求中位数约 1 ms，本机回环约 0.4 ms，2026-10-09）：

| 字段 | 含义 |
|---|---|
| `missing_count` | 该根下（按路径前缀，含子目录）`missing = 1` 的记录数；根尚未扫描时为 `0` |
| `last_scan` | 该根**或其下子目录**最新的一次扫描会话（库里的扫描历史，含其他进程做的扫描），没有时为 `null`。嵌套配置的根（一个根在另一个根下面）只看扫描路径在自己之内的会话，上层根的扫描不算。字段：`id`（会话 id，可用于 `GET /scans/{id}`）、`subpath`（相对根，`""` = 整个根）、`mode`（`incremental` \| `full`）、`state`（库里记录的 `running` \| `completed` \| `cancelled` \| `failed`；`running` 但本服务端没在扫描 = 中断或另一进程在扫描，见 §6.6）、`started_at` / `finished_at`（Unix 秒，未结束为 `null`）、`scanned`、`added`、`updated`、`removed`、`unchanged`、`errors` |
| `scanning` | 本服务端正在扫描（含暂停中）该根或其下子目录时为 `true`；其他进程的扫描不反映在这里 |

示例（合成数据）：

```json
[{ "name": "SFX_Main", "server_path": "/srv/audiolib/music/SFX_Main", "webdav_url": null,
   "file_count": 12, "missing_count": 0, "online": true, "status": "online", "status_message": null,
   "checked_at": 1700000001, "scanning": false,
   "last_scan": { "id": 2, "subpath": "", "mode": "incremental", "state": "completed",
                  "started_at": 1700000000, "finished_at": 1700000001, "scanned": 12, "added": 2,
                  "updated": 0, "removed": 0, "unchanged": 10, "errors": 0 } }]
```

### 6.5 增量同步 `GET /library/changes`（⚠️ 草案，待客户端评审，未实现）

> **DRAFT / 草案**：本节只是提案，服务端尚未实现，字段和错误码都可能改。客户端评审通过后再实现。在此之前客户端继续用 §4 的“revision 变化 → 下载快照”（快照现已按 revision 缓存并支持 zstd，§6.1）。

目的：`revision` 变化后只拉取变化的记录，而不是重新下载整个快照。

**请求**：`GET /library/changes?since=<revision>&limit=<1..=2000，默认 500>[&cursor=<不透明字符串>]`

- 第一页只带 `since`（客户端本地缓存对应的 `revision`）；后续页只带上一页返回的 `cursor`（`since`/`limit` 可省略，以游标为准）。

**响应**（200）：

```json
{
  "library_id": "00000000-0000-4000-8000-000000000001",
  "since": 120,
  "until": 135,
  "changes": [
    { "seq": 9001, "kind": "audio_upsert",  "id": 66869, "record": { "...": "完整 AudioFile" } },
    { "seq": 9002, "kind": "audio_delete",  "id": 70001, "record": null },
    { "seq": 9003, "kind": "folder_upsert", "id": 412,   "record": { "...": "完整 Folder" } },
    { "seq": 9004, "kind": "folder_delete", "id": 413,   "record": null },
    { "seq": 9005, "kind": "roots_changed", "id": null,  "record": null }
  ],
  "has_more": true,
  "next_cursor": "opaque…"
}
```

- `until`：这组变化**完整覆盖到**的 revision。第一页就确定 `until`，后续页沿用同一个 `until`（游标里固定了上界），所以分页期间新发生的改动不会混进来，也不会漏掉——它们属于下一次 `since=<until>` 的请求。
- **客户端规则**：依次应用所有页（直到 `has_more = false`），全部成功后把本地 `revision` 设为 `until`。中途失败就丢弃已应用的部分（或保留，因为每条都是记录的最新完整状态，重复应用是幂等的），下次从原来的 `since` 重来。
- 覆盖范围：
  - `audio_upsert`：`audio_files` 的新增和任何列的修改（metadata 编辑、播放计数、扫描更新、`missing` 标记变化、relink），`record` 为当前完整 `AudioFile`；
  - `audio_delete`：记录被真正删除（`remove_audio` / `remove_folder`）；
  - `folder_upsert` / `folder_delete`：`folders` 的新增、修改（含 `file_count`）、删除；
  - `roots_changed`：服务端配置的根（增删、`server_path`/`webdav_url` 变化）或根状态以外的根信息变化，客户端重新 `GET /roots`；
  - 统计（`stats`）不单独传，客户端由记录重算或重新 `GET /library`。
- 同一实体在区间内多次变化只返回一条（最新状态，`seq` 取最后一次），所以总条数 ≤ 变化的实体数。
- **退回快照**：`since` 早于服务端保留的变更日志、`since > 当前 revision`、或 `library_id` 不符时返回 **410 `CHANGES_EXPIRED`**，客户端改为下载快照。服务端也可以在变化量超过阈值（例如 > 20% 记录）时返回 410，让客户端直接下快照。
- 实现设想（供讨论）：未来 schema 版本（v4 为索引升级、v5 为评分接口移除与提取标签派生状态，见 §3.1）在库里加 `change_log(seq INTEGER PRIMARY KEY, entity, entity_id, op)` 和 `audio_files` / `folders` 上的触发器（这样服务端之外的写入者，例如 CLI 扫描器，也会被记录）；服务端在每次 `revision` 递增时记下“revision → 当时最大 seq”，`since`/`until` 由此换算成 seq 区间；日志按时间或条数裁剪。schema v3 期间无法捕获外部写入，所以本端点依赖尚未实现的未来 schema。

### 6.6 远程扫描 `/scans`（已实现）

服务端用工作区里的扫描器（`audiolib_db::ScanService`，与本地模式同一套代码）在独立的 OS 线程上扫描配置的根（或其下的子目录），不占异步运行时、没有时间上限。进度就是本地模式的 `ScanProgress`（字段名完全相同），外加几个服务端字段。扫描只读源文件，只写库，永远不改 WAV。按 §2.2 鉴权（属于写入端点：局域网直连免 token，外部请求需要 token）；`POST /scans` 和暂停/继续/取消另受库管理权限 `manage_mode` 限制（§6.9，被拒绝时 403 `MANAGE_FORBIDDEN`），查询进度的 GET 不受限；库未就绪时 503 `UNAVAILABLE`。

#### `POST /scans`

请求体（JSON，最大 8 MiB，必须声明 `Content-Type: application/json`，可带 `charset`）：

```json
{ "root": "SFX_Main", "subpath": "Foley/Doors", "mode": "incremental", "workers": null }
```

| 字段 | 说明 |
|---|---|
| `root` | **必填**，`GET /roots` 里的 `name`。未知根名 → 404 `NOT_FOUND` |
| `subpath` | 可省略、`null` 或 `""` = 整个根。相对于根、用 `/` 分隔（连续或末尾的 `/` 忽略）。以 `/` 开头、含 `\`、含 `.` 或 `..` 分量、含 NUL 或超过 4096 字节 → 400 `VALIDATION`。服务端解析真实路径（跟随符号链接）后必须仍在根内，否则 400 `VALIDATION`（`…resolves outside library root…`）；不存在或不是目录 → 404 `NOT_FOUND` |
| `mode` | `incremental`（默认）：只读取新文件和大小/修改时间变化的文件，从不标记 missing；`full`：重新读取每个文件，无错误完成的全量扫描把扫描范围内没找到的记录标为 missing（不删除记录）。其他值 → 400 |
| `workers` | 读取元数据的线程数 `1..=16`；省略、`null` 或 `0` = 自动（CPU 数 − 2，限制在 1..8，与本地模式相同）。其他值 → 400 |

响应 200（`ScanResponse`，扫描会话在库里建好、拿到 `session_id` 后立即返回，通常不到 1 秒）：

```json
{
  "session_id": 17, "state": "running", "paused": false, "finalizing": false,
  "discovered": 0, "processed": 0, "added": 0, "updated": 0, "unchanged": 0, "removed": 0,
  "errors": 0, "error_details": [], "errors_truncated": 0,
  "current_path": "/srv/audiolib/music/SFX_Main/Foley/Doors", "message": "",
  "root": "SFX_Main", "subpath": "Foley/Doors", "mode": "incremental",
  "active": true, "pause_requested": false, "cancel_requested": false, "revision": null
}
```

| 服务端字段 | 含义 |
|---|---|
| `root` / `subpath` | 根名和规范化后的子目录（`""` = 整个根）；来自历史的会话若扫描路径不在任何配置的根下则为 `null` |
| `mode` | `incremental` \| `full` |
| `active` | **本服务端进程正在执行这个会话**（运行或暂停中）。轮询直到它变为 `false` |
| `pause_requested` | 已请求暂停且尚未继续；工作线程在安全点真正停下后 `paused`（`ScanProgress` 原有字段）才变为 `true`，通常不到 0.2 秒 |
| `cancel_requested` | 已请求取消 |
| `revision` | 扫描结束后库的 `revision`（运行中、以及来自历史的会话为 `null`），见下面的 revision 规则 |

`ScanProgress` 原有字段：`state`（`running` \| `completed` \| `cancelled` \| `failed`）、`finalizing`（已开始最终的原子对账，之后不再接受控制）、`discovered`/`processed`（找到/处理的 WAV 数）、`added`、`updated`、`unchanged`、`removed`（标为 missing 的记录数）、`errors` + `error_details`（最多 100 条 `{path, message}`，其余计入 `errors_truncated`）、`current_path`（服务端绝对路径，同 §3 的路径暴露约定）、`message`。

规则：

- **返回时机与超时**：返回前服务端要解析根目录和 `subpath`（NAS 硬盘休眠时可能要 20–30 秒或更久），所以 `POST /scans` **不受 30 秒限制**。客户端放弃等待（断开）时扫描照常开始，用 `GET /scans/current` 找回。
- **根离线**：根在状态缓存里为 `offline` 时先做与 `/stream` 相同的有界即时重查（约 2 秒，§6.4），仍离线 → 503 `ROOT_OFFLINE` + `Retry-After: 5`（`message` 为 `Library root "<根名>" is offline: <原因>`）；扫描线程解析根目录时失败（卷未挂载、无权限、网络超时）也返回 503 `ROOT_OFFLINE`。这些情况都不建会话、不改库。
- **同一时间只有一个扫描**：本服务端已有扫描（运行或暂停中，含刚开始解析目录的）→ 409 `BUSY`（`message` 含会话 id）；另一个进程（CLI、本地模式桌面端）正在扫描或维护同一个库 → 409 `BUSY`。扫描期间 `POST /audio/metadata/batch` 返回 409 `BUSY`（§6.3，与本地模式相同）；单条 `PATCH /audio/{id}/metadata` 和 `POST /audio/{id}/plays` 照常可用。
- **慢盘、空根、出错（§6.4 的规则不变）**：根目录不存在/不可读时扫描直接失败、不改记录；扫描途中目录读取出错（例如卷断开）、遇到符号链接（不跟随）、被取消或失败时都**不做 missing 对账**；全量扫描遇到完全为空的根（典型的未挂载挂载点）也不对账；慢只会让扫描变慢。单个文件读不出来记一条 `error_details`，扫描继续（但全量扫描只要有任何错误就不做 missing 对账）。
- **子目录扫描**：扫描范围是 `subpath` 之下；它的文件夹记录挂在所属根的文件夹树下（必要时先建好从根到该子目录的文件夹记录），不会变成单独的顶层文件夹。全量子目录扫描只对该子目录范围做 missing 对账。
- **停机 / 重新部署**：服务端收到停止信号时立即取消正在进行（含暂停中）的扫描，最多等 10 秒让它把会话记为 `cancelled`（已提交的批次保留；每批一个事务，库不会损坏；不做 missing 对账）。停止期间新的 `POST /scans` → 503 `UNAVAILABLE`。若扫描线程卡在 NAS 上 10 秒内没结束，进程照常退出，库里该会话保持 `running`，直到下一次扫描开始时被标为 `failed`（`Previous scanning process exited before finalization`）；`GET /scans/{id}` 对这种会话返回 `active: false`，`message` 说明它不在本服务端运行。

#### 进度：`GET /scans/{session_id}` 与 `GET /scans/current`

- **轮询**：开始扫描后每 **500 ms** `GET /scans/{session_id}`，直到 `active` 为 `false`，再看 `state` 和 `revision`。暂停中也照常轮询（进度不动）。
- `GET /scans/current` 始终 200：`{ "scan": ScanResponse }`，没有扫描时 `{ "scan": null }`（不是 404）。客户端连上/重连服务器时用它恢复扫描进度界面。`scan.session_id == 0` 表示扫描正在解析目录、会话还没建好（极短；NAS 唤醒时可能较久）。
- 会话保留：本进程最近结束的 32 个会话保留完整状态（含 `root`、`subpath`、`revision`）。更早的、或服务端重启之前的会话从库的扫描历史读出（只有计数：`discovered` = `processed` = `scanned`，`revision` 为 `null`，`root`/`subpath` 由扫描路径推算）。不存在的会话 → 404 `NOT_FOUND`；非正整数 id → 400 `VALIDATION`。

#### 控制：`POST /scans/{session_id}/pause` · `/resume` · `/cancel`

无请求体，200 返回操作后的 `ScanResponse`。重复已经生效的操作返回 200（幂等，网络重试安全）：

| 操作 | 效果 |
|---|---|
| `pause` | `pause_requested` 变为 `true`，工作线程在安全点停下后 `paused` 变为 `true`。暂停中会话和扫描锁都保留（其他扫描仍 409，`revision` 仍冻结） |
| `resume` | 继续同一个扫描（队列和计数不变）；未暂停时不变 |
| `cancel` | `cancel_requested` 变为 `true`，扫描在当前文件后停止（暂停中也可以直接取消）；已提交的批次保留，不做 missing 对账，最终 `state` 为 `cancelled` |

**409 `SCAN_STATE`**（`message` 说明原因）：会话已结束（`completed`/`cancelled`/`failed`）；会话不在本服务端运行（历史里的 `running`）；正在取消时 `pause`/`resume`；已进入最终对账（`finalizing: true`）后 `pause`/`resume`/`cancel`。未知会话 404。（旧客户端把所有 409 当作 `Busy` 处理，同样可用。）

#### revision 规则

- **扫描期间 `revision` 冻结**：扫描分批提交（每批 128 个文件一个事务），但 `/library` 轮询不会每批递增，客户端不会在扫描中途反复下载整库快照。扫描期间请求的快照标的是冻结的 `revision`，内容不旧于它（可能已含部分扫描结果）。
- **结束（完成、取消或失败）时最多递增一次**：扫描有改动（`added`、`updated`、`removed` 任一 > 0，或文件/文件夹/missing 统计变化），或扫描期间有其他进程写过库 → `revision` +1，按 revision 缓存的快照随之失效；否则**不递增**（扫描会话记录和“本次已见”标记的写入不算改动）。因此：**无改动的增量扫描不递增**；全量扫描会重写每条记录（`updated` = 文件数），**总会递增**。
- 结束后的 `revision` 在响应的 `revision` 字段里；与客户端缓存的 `revision` 不同就按 §4 同步快照（下一次 `/library` 轮询也会看到）。
- 扫描期间成功的单条 metadata 写入照常递增：`revision_before` = 冻结值 + 1（已包含此前的扫描提交），`revision` = 冻结值 + 2；客户端按 §4 规则：`revision_before` 与本地不同就做快照同步。被拒绝的写入（例如 409 `BUSY` 的批量写入）不改变 `revision`。
- 快照里也有扫描会话表，但无改动的扫描不递增 `revision`，所以本地快照里的扫描历史可能落后；**显示扫描历史请用 `GET /history/scans`**（§6.7）。

### 6.7 历史 `GET /history/scans`、`GET /history/plays`（后者已弃用）

- 查询参数：`limit`（默认 100，`1..=10000`）、`offset`（默认 0，非负整数）；超出范围、不是整数或重复 → 400 `VALIDATION`。按 §2.2 鉴权，只读库，30 秒超时。
- `GET /history/scans` → `ScanSession[]`，按会话 id 从新到旧：

  ```json
  [{ "id": 17, "start_time": "2026-10-08 17:20:31.120", "end_time": "2026-10-08 17:20:33.004",
     "scan_path": "/srv/audiolib/music/SFX_Main/Foley/Doors", "scan_type": "incremental",
     "state": "completed", "scanned": 12, "added": 1, "updated": 0, "removed": 0, "unchanged": 11,
     "errors": 0, "error_details": [], "errors_truncated": 0 }]
  ```

- `GET /history/plays` → `[]`，保留鉴权与分页参数校验，不再读取播放历史：

  ```json
  []
  ```

- 扫描历史是 `audiolib-core` DTO 原样（与本地模式 `scan_history` 相同）；其中的时间是 SQLite 记录的 **UTC 文本**（`YYYY-MM-DD HH:MM:SS.fff`），不是 Unix 秒。`ScanSession.state` 为 `running` 而本服务端没有在扫描（`GET /scans/current` 为 `null`）时表示那次扫描被中断或由另一进程执行（见 §6.6 停机）。

### 6.8 从库中移除：`POST /audio/remove`、`DELETE /folders/{id}`（已实现）

**只让库“忘记”记录，从不删除、移动或修改任何音频文件。** 文件仍在磁盘上时，下一次扫描（增量或全量）会把它们重新加入库中（得到**新的 id**，评分、标签、备注、颜色、收藏、播放次数等不会恢复）。客户端确认文案统一用：

> 从库中移除（不删除文件，重新扫描会恢复）

两个端点共同的规则：

- 按 §2.2 鉴权，再受库管理权限 `manage_mode` 限制（§6.9；被拒绝时 403 `MANAGE_FORBIDDEN`）。检查顺序：鉴权 401 → `manage_mode` 403 → 兼容目录/实际格式 409（§2）→ 请求参数 400 → 扫描占用 409 → （文件夹）不存在 404 → 库根顶层文件夹 400。
- **扫描期间不可用**：本服务端有扫描在运行或暂停中（§6.6），或另一个进程（CLI、本地模式桌面端）正在扫描/维护同一个库 → 409 `BUSY`（`message` 含会话 id），即使请求的 id 都不存在也是 409。客户端稍后重试，或先取消/等扫描结束。
- 被移除记录的播放列表条目、工程引用和主库波形缓存行一并删除，扫描历史不受影响。v6 的播放历史独立保存在客户端本机，不随服务端或本地库记录移除而删除；历史条目保留，无法定位当前素材时显示缺失。`/history/plays` 始终为 `[]`，不代表本机历史已清空。
- **revision**：与 metadata 写入相同的临界区（§4）；真正删除了东西时 `revision = revision_before + 1`，按 revision 缓存的快照随之失效；什么都没删时不写库、`revision == revision_before`。
- 受 30 秒请求超时约束（实测一次较大批量删除约 0.5 秒，生产记录量已从公开记录移除）。

#### `POST /audio/remove`

```json
{ "ids": [5, 999999999, 5, 6] }
```

- `ids`：1..=10000 个正整数（按原始数组计数，含重复）；空数组、超过 10000、非正数、不是整数数组或不是合法 JSON → 400 `VALIDATION`。重复的 id 自动去重。
- 不存在的 id 不算错误，原样（按请求顺序、去重后）放进 `not_found`。
- 响应 200（测试实例的真实输出）：

```json
{ "removed": 2, "not_found": [999999999], "revision_before": 2, "revision": 3 }
```

  再发一次同样的 id（都已不存在）：`{ "removed": 0, "not_found": [5, 999999999], "revision_before": 3, "revision": 3 }`。

- 错误示例：`{"error_code":"VALIDATION","message":"ids must contain 1..=10000 audio ids, got 10001"}`、`{"error_code":"VALIDATION","message":"ids must be positive integers, got 0"}`。

#### `DELETE /folders/{id}`

- 移除该文件夹**整个子树**：其中（含所有子文件夹）的全部音频记录、这些文件夹记录本身，以及文件夹闭包表；之后刷新文件夹统计（`/roots.file_count` 随之变化）。`id` 来自 `Folder.id`（快照中的 `folders` 表）。
- 非整数 id → 400 `VALIDATION`（同 `/audio/abc`）；不存在 → 404 `NOT_FOUND`。
- **库根的顶层文件夹**（`[[roots]].server_path` 对应的文件夹），以及包含某个库根的上级文件夹 → 400 `VALIDATION`：库根不能经 `DELETE /folders/{id}` 删除；使用 §6.10 的 `DELETE /roots/{name}` 或管理页 `/admin` 的「库根目录」（§2.3.1）。
- 响应 200（`folders_removed` = 删除的文件夹数，含它自己）：

```json
{ "removed": 2, "folders_removed": 2, "revision_before": 3, "revision": 4 }
```

  即使子树里没有音频（`removed: 0`），文件夹记录也被删除，`revision` 照样 +1。

- 错误示例：

```json
{ "error_code": "NOT_FOUND", "message": "Not found: Folder 999999999" }
{ "error_code": "VALIDATION", "message": "Folder 1 is the top-level folder of library root \"Scratch\" (or contains it) and cannot be removed through the API; remove the library root on the server's admin page (管理页 /admin → 库根目录)" }
{ "error_code": "BUSY", "message": "Busy: A scan is running for this library (session 2); retry when it has finished" }
```

### 6.9 库管理权限 `manage_mode` 与 `GET /capabilities`（已实现）

服务端设置 `manage_mode` 决定谁可以**管理库**：`POST /scans`、`POST /scans/{id}/pause|resume|cancel`、`POST /audio/remove`、`DELETE /folders/{id}`，以及 §6.10 的 `GET /fs`、`POST /roots`、`PATCH /roots/{name}`、`DELETE /roots/{name}`，以及 §9.5 的列表波形任务端点（`POST /waveforms/jobs`、`POST /waveforms/jobs/current/pause|resume|stop`，草案）。Metadata 写入（`PATCH /audio/{id}/metadata`、`POST /audio/metadata/batch`）、播放计数（`POST /audio/{id}/plays`）和其他 GET 不受影响。

| `manage_mode` | 局域网直连 | 外部请求（经反代 / 公网对端，判定同 §2.2） |
|---|---|---|
| `all` | 允许 | 允许（仍需按 §2.2 带 token） |
| `lan`（**默认**） | 允许 | 403 `MANAGE_FORBIDDEN` |
| `off` | 403 `MANAGE_FORBIDDEN` | 403 `MANAGE_FORBIDDEN` |

- 在管理页「库管理权限」设置（保存在 `admin-settings.json`，立即生效，§2.3），或 `config.toml` 的 `manage_mode`（管理页优先）。
- 与鉴权模式独立：先按 §2.2 鉴权（缺 token 仍是 401），通过后才检查 `manage_mode`。
- 403 示例：`{"error_code":"MANAGE_FORBIDDEN","message":"Library management (scans, removing records or folders) is only allowed from the LAN on this server (manage_mode = lan)"}`；`off` 时为 `… is disabled on this server (manage_mode = off)`。

`GET /capabilities` 让客户端决定是否显示/启用扫描和移除功能：

- 按 §2.2 鉴权（与其他端点相同，不并入免鉴权的 `/health`）；不需要库已打开，服务端启动中也返回 200。30 秒超时。
- 响应 200：

```json
{ "manage_mode": "lan", "external": true, "can_manage": false, "root_management": true }
```

| 字段 | 含义 |
|---|---|
| `manage_mode` | 当前生效的 `all` \| `lan` \| `off` |
| `external` | 服务端是否把**这个请求**当作外部请求（§2.2 的判定） |
| `can_manage` | 这个请求（同样的网络路径）调用上述管理端点是否会被允许 |
| `root_management` | 本服务端提供 §6.10 的稳定根目录管理接口；缺字段的旧版本按 `false`，客户端隐藏目录浏览及根目录新增、改路径、删除按钮 |
| `list_waveforms` | 支持 §9.1 的 `POST /waveforms`：`{ "version": 1, "buckets": [128], "max_ids": 200, "encoding": "i16le-minmax-base64" }`；缺字段 = 不支持 |
| `pregenerate_list_waveforms` | 当前生效的列表缩略波形预生成开关（只读），修改用 `PATCH /settings`，见 §9.2 |
| `list_waveform_jobs` | 支持 §9.5 的列表波形缓存管理任务与 `list_waveform_rate` 设置：`{ "version": 1, "modes": ["missing", "rebuild", "failed"], "rate_setting": true }`；缺字段 = 不支持（库已打开但 `waveforms.db` 无法打开时也不声明，§9.5.7） |
| `compatibility.features.album_artwork` | 可选 feature，修订 ≥ 1 表示支持 §8.2 的 `GET /audio/{id}/artwork`。键名就是 `album_artwork`。旧服务端没有该键（或修订不同）时客户端不要请求封面；缺失只关闭这一功能，不拒绝同步 |
| `compatibility.features.album_entity_artwork` | 可选 API feature，修订 1 表示支持 §8.2 的 `GET` / `HEAD /albums/{id}/artwork`（共享专辑实体封面，须带 `X-AudioLib-Library-Id` 和 `X-AudioLib-Album-Namespace`）。缺失时客户端保留按音频的 `/audio/{id}/artwork`；缺失只关闭这一功能，不拒绝同步 |
| `compatibility.features.album_folder_artwork` | 可选 API feature，修订 1 表示 `/albums/{id}/artwork` 支持 `folder_id` 或 `unassigned_directory` 的直属目录过滤及身份回显；不改变数据库格式、schema 或迁移要求 |

- §9.2 的 `PATCH /settings` 和 `POST /waveforms/pregeneration/pause|resume` 同样受 `manage_mode` 限制（`GET /settings`、`GET /waveforms/status`、`POST /waveforms` 不受限制）。
- 设置随时可能在管理页被改；`can_manage` 只是提示，管理端点仍可能返回 403 `MANAGE_FORBIDDEN`，客户端按错误码处理即可。旧服务端没有该端点（404）时按“只读、不提供管理功能”处理。

### 6.10 客户端远程目录浏览与音效库根目录管理（已实现）

客户端使用 `/api/v1` 的本节接口，管理页继续使用 §2.3.1 的 LAN 专用接口。所有本节接口先按 §2.2 鉴权，再检查 `manage_mode`（拒绝为 403 `MANAGE_FORBIDDEN`）；新增、改路径、删除还必须携带当前快照的 `X-AudioLib-Library-Id`，缺失为 409 `LIBRARY_ID_REQUIRED`、与服务器当前库不一致为 409 `LIBRARY_CHANGED`。浏览器修改请求仍须同源。只管理当前连接所选服务器的音效库根目录，不移动服务器数据库文件。

`GET /fs` 不带 `path` 返回 `{"places":[{"kind":"root|volume|home|library_root","name","path","network":true|false|null}]}`。`GET /fs?path=<绝对路径>[&hidden=1]` 返回 §2.3.1 相同的目录列表：`path`、`parent`、`network`、`entries[{name,path,is_symlink}]`、`truncated`、`wav_files`、`other_files`、`counts_truncated`。进入目录前解析真实路径；只列直接子目录，从不写入文件、创建目录或递归扫描。每次最多返回 2000 个子目录、检查 50000 个条目；起点的 `/Volumes` 枚举也最多检查 2000 项。浏览和根目录路径检查共用 4 个阻塞任务名额；浏览等待 15 秒后返回 504 `FS_TIMEOUT`，卡住的系统调用继续占其名额，名额满时 503 `BUSY` + `Retry-After: 1`。相对/空路径或非目录为 400 `VALIDATION`，目录不存在为 404 `NOT_FOUND`，拒绝读取为 403 `FS_FORBIDDEN`。

`POST /roots` 请求 `{"name":"SFX_New","path":"/Volumes/Sounds/New"}`，成功 201，返回 `GET /roots` 的一个条目。名称和路径校验、根目录互不重叠规则同 §2.3.1；路径原样处理，保留有效文件名空格。根目录设置写入 `admin-settings.json`，即时生效并覆盖 `config.toml` 根列表；不会自动扫描。扫描运行/暂停时返回 409 `BUSY`。路径检查仍有 15 秒上限，超时或请求取消后不会迟到登记根目录。

`PATCH /roots/{name}` 请求 `{"path":"/Volumes/Relocated/SFX"}`，成功 200 返回更新条目，名称保持不变。未扫描过的根只更新配置；已入库的根按原相对路径逐一验证目标目录和 WAV：每个目标必须真实存在、处于所选新根内、不经过目录链接、不占用其他记录，WAV 文件大小及采样率、声道、位深、时长须匹配已索引 Metadata。缺失/无效/不匹配目标会在提交前拒绝整个请求；此校验不声称验证 WAV 全部 PCM 字节相同。文件系统根和 Windows 盘符根不支持改路径，旧根存在多个存储路径身份时也拒绝，均为 400 `VALIDATION`。改路径前创建库目录 `backups/before-root-relink-*.db` 回滚备份，并持有扫描和独占管理 OS 锁；文件路径、文件夹路径在同一事务中更新，保留音频及文件夹 ID、Metadata、用户标注、歌单、项目引用、扫描及播放历史，历史路径保留当时记录。清除 Waveform 数据库缓存、刷新目录统计，库 revision 增加一次；实际路径改变后旧 `webdav_url` 清空，避免继续指向原资源。源 WAV 始终只读。

`DELETE /roots/{name}` 回复同 §2.3.1 的移除报告。数据库单次事务清理根目录下的音频、文件夹及依赖记录，随后更新根列表；源 WAV 永不删除。移除已索引根之前保存 `backups/before-root-remove-*.db` 回滚备份。扫描运行/暂停时 409 `BUSY`，未知根 404；多个旧路径身份的根拒绝为 400。未扫描根没有记录可移除，revision 不变。

SQLite 事务与根设置使用持久化恢复日志配对：`admin-settings.json` 的 `root_relink` / `root_removal` 在事务前保存，成功后清除；进程中断或最后设置写盘失败时，重启及下一次根修改根据保存的文件夹 ID 与数据库已提交状态恢复正确根列表。根修改、恢复及普通管理设置写入共用设置目录的 `.admin-settings.lock` OS 锁，侧文件解锁后保留；占用时返回 409 `BUSY`，另一实例不会清除进行中操作的日志。持锁后重新读取最新根列表，避免旧实例覆盖新配置。即使恢复设置仍无法写盘，已运行服务器也按数据库提交状态更新根监控，日志保留供后续重试。PATCH 与 DELETE 没有通用 30 秒请求上限：大量记录的验证、备份或删除可耗时更长。PATCH 请求取消会在提交前回滚；DELETE 在开始数据库写入前检查取消，开始后排空已请求的移除。客户端对可能已提交的失败也重新同步当前服务器快照与根列表，不能将超时当作确定未写入。

## 7. WebDAV 与音频访问

> **WebDAV：功能未完善。** 下面的约定和现有字段保持不变（`/roots.webdav_url` 仍可为 `null`，服务端只给建议默认值，地址由客户端用户填写），服务端不提供任何 WebDAV 端点，也不保证 WebDAV 路径可用；试听和拖出请走 §8 的服务端直连。

- **局域网服务器模式的默认路径是服务端直连**：试听走 `GET /audio/{id}/stream`，文件传输（拖出、导出）走 `GET /audio/{id}/stream?download=1`（§8）。WebDAV 是**可选项**，不配置也能完整使用试听和拖出。
- 客户端拖出文件：先用 `download=1` 下载到本地缓存（键 = `library_id` / 文件 id + `ETag`，缓存文件使用原始文件名，即 `Content-Disposition` 的 `filename*`），完成后把这个真实本地路径交给 Windows 拖放；缓存命中时可用 `HEAD` 或 `If-None-Match` 复核 `ETag`，下载中断可用 `Range` + `If-Range` 续传。
- 服务端**不自带 WebDAV**，使用 Mac 上已有的 WebDAV 服务（NAS / 文件共享等）发布扫描根目录。
- 按用户决定：每个扫描根的 WebDAV 地址在客户端**可由用户编辑**。服务端 `[[roots]].webdav_url` 只是建议默认值，可以不配置（`/roots` 返回 `null`）。
- 客户端把 `AudioFile.file_path` 的 `server_path` 前缀替换为该根的 WebDAV 地址后读取音频。

## 8. 音频流（已实现）

`GET /audio/{id}/stream`（按 §2.2 鉴权；也支持 `HEAD`，只返回响应头）返回原始文件字节，不转码，客户端负责解码、重采样和输出（与本地模式一致）；低带宽试听用 §8.1 的 `/audio/{id}/preview`。客户端 Symphonia 解码器通过 HTTP Range 实现可 seek 的读取（RIFF/RF64 头部读取、ADPCM 按块 seek 都依赖随机访问）。WebDAV（§7）仍可继续使用。

- **响应头**（200/206/304/416 都带）：`Accept-Ranges: bytes`、`ETag`、`Cache-Control: private, no-cache`。200/206 另带 `Content-Type`、`Content-Length`；206 带 `Content-Range: bytes <start>-<end>/<文件长度>`。
- **`Content-Type`**：按扩展名，`.wav`/`.wave`/`.bwf`/`.rf64` → `audio/wav`（当前库只有 WAV）；`.aif(f)`/`.aifc` → `audio/aiff`、`.flac` → `audio/flac`、`.mp3` → `audio/mpeg`、`.ogg`/`.oga` → `audio/ogg`、`.m4a`/`.mp4` → `audio/mp4`；其他 `application/octet-stream`。
- **Range**（只支持单个区间）：
  - `bytes=a-b` → 206，`b` 超出文件末尾时截到最后一个字节；`bytes=a-` → 206，从 `a` 到末尾；`bytes=-n` → 206，最后 `n` 个字节（`n` 大于文件长度时为整个文件）。
  - 起点 ≥ 文件长度、`bytes=-0`、或文件长度为 0 → **416** `RANGE_NOT_SATISFIABLE`，附 `Content-Range: bytes */<文件长度>`，JSON 错误体。
  - **多区间**（如 `bytes=0-1,5-6`）不实现 `multipart/byteranges`，直接返回 **200 + 整个文件**；单位不是 `bytes` 或语法非法（如 `bytes=9-2`）的 `Range` 按 RFC 9110 忽略，同样返回 200 整个文件。客户端应以状态码（206 还是 200）判断收到的是区间还是全文件。
- **`ETag`**：强校验器，由服务端打开的那个文件的大小 + 修改时间计算，Unix（含 macOS 服务端）上另含 inode 和 ctime（同大小同 mtime 的替换文件也会得到新 ETag），Windows 另含创建时间；客户端把它当作不透明的带引号字符串。库记录里没有“提取版本”字段，所以 v1 不含提取版本（以后加入时 ETag 会整体变化，客户端无需改动）。不发送 `Last-Modified`。
  - `If-None-Match`：任一标签（弱比较，或 `*`）匹配 → **304**，无响应体。
  - `If-Range`：只接受与当前 `ETag` 完全相同的强 ETag，此时按 `Range` 返回 206；不匹配、弱 ETag（`W/"…"`）或 HTTP 日期 → 忽略 `Range`，返回 **200 + 整个文件**。
- **只提供配置的根下的文件**：服务端把库里的 `file_path` 规范化（解析符号链接和 `..`），只有落在某个 `[[roots]] server_path`（同样规范化，按路径分量比较，每次请求时重新解析，因此启动后才挂载的卷也有效）之下的普通文件才会返回；Unix 上打开后还核对打开的文件与检查过的路径是同一个文件（设备号 + inode），不一致也拒绝。根之外的路径返回与文件缺失**完全相同**的 404，不泄露根之外的文件是否存在。
- **404 `NOT_FOUND`**：无此记录；或文件缺失、不是普通文件、不在配置的根下（后三种 `message` 相同，均含 `missing`）。
- **根离线 / 慢速磁盘**：文件所在的根离线时先即时重查（约 2 秒），仍离线则返回 503 `ROOT_OFFLINE` + `Retry-After: 5`（§6.4）；`HEAD` 同样触发重查。本端点没有时间上限，NAS 硬盘唤醒期间请求会一直等待，不会 408。
- **并发上限**：同时进行中的流响应最多 `max_streams` 个（服务端配置，默认 16；名额一直占用到响应体发完、客户端断开，或客户端连续 60 秒不接收数据，见 §2.1 末尾的“连接与超时”）。超出时返回 **503 `BUSY`** + `Retry-After: 1`，客户端按 `Retry-After` 稍后重试。304/416 不占名额。§8.1 的预览**不占**这些名额（有自己的 `max_previews`），所以拖出下载等把 `/stream` 名额占满时预览照常可用。
- **响应体不完整**：发送途中文件被截断或连接中断时，客户端收到的字节数会少于 `Content-Length`（或 `Content-Range` 所示长度），应视为失败并重试（可用 `If-Range` + 已收到部分续传）。文件缺失时，下次重扫后 `GET /audio/{id}` 的 `missing` 为 true（流端点本身不改库）。
- 服务端用 tokio 文件 + 分块流（256 KiB）发送，不会把整个文件读入内存。
- **下载 `?download=1`**（也接受 `download=true`，大小写不敏感；不带该参数为默认的播放用途，响应不带 `Content-Disposition`，即按 inline 处理；`download` 的其他取值、重复或格式错误的查询串 → 400 `VALIDATION`，其他未知参数忽略）。文件选择、根目录限制、404、并发上限、`Range`/`If-Range`/`If-None-Match`/304/416 与上面完全相同（`Range` 照常生效，用于续传中断的下载），唯一区别是 200/206/304 响应多一个头：

  ```
  Content-Disposition: attachment; filename="<ASCII 回退名>"; filename*=UTF-8''<RFC 5987 百分号编码的原始文件名>
  ```

  例如库里文件名为 `雷声 近.wav`（id 7）：`Content-Disposition: attachment; filename="audio-7.wav"; filename*=UTF-8''%E9%9B%B7%E5%A3%B0%20%E8%BF%91.wav`；`door.wav` 则为 `attachment; filename="door.wav"; filename*=UTF-8''door.wav`。原始文件名取库记录的 `filename`。ASCII 回退名：非可打印 ASCII 字符以及 `"` `\` `/` `:` `*` `?` `<` `>` `|` 换成 `_`；主名里没有 ASCII 字母数字时改为 `audio-<id>`（保留 ASCII 扩展名）；最长 150 个字符。`filename*` 按 RFC 5987 对 UTF-8 字节编码（`attr-char` 之外的字节一律 `%XX`，大写十六进制），客户端应优先使用 `filename*`。
  
  `download=1` 的三条保证（有测试覆盖）：
  1. **`ETag` 与不带参数的 `/stream` 逐字节相同**（同一文件版本），`Content-Disposition` 不参与 `ETag` 计算，所以缓存、`If-None-Match`、`If-Range` 在播放和下载之间通用。
  2. **200/206 总是带 `Content-Length`**；`HEAD ...?download=1` 返回与 `GET` 相同的响应头（含 `Content-Length`、`ETag`、`Content-Disposition`、`Accept-Ranges`），没有响应体。
  3. **`Content-Disposition` 同时带 `filename=`（ASCII 回退）和 `filename*=UTF-8''…`**，中文等非 ASCII 文件名不丢失。

### 8.1 试听预览 `GET /audio/{id}/preview`（已实现）

用途：低带宽（目标 5 Mbps）下流畅试听。例如 96 kHz/24-bit 的立体声 PCM WAV 约需 4.6 Mbps，预览可降低传输量；原始文件仍走 `/stream`（拖出、导出、精确播放）。按 §2.2 鉴权；支持 `HEAD`（只返回响应头）。两种编码：

| `codec` | 实现 | 长度 / Range | 定位方式 | 适用 |
|---|---|---|---|---|
| `pcm`（**首选**） | 服务端进程内：解析 WAV → 整数倍抽取（抗混叠 FIR）→ 折叠声道 → 量化（确定性 TPDF 抖动），输出 44 字节头的标准 WAV；不用 ffmpeg | 总长已知：`Content-Length`、`Accept-Ranges: bytes`、单区间 `Range`（206/416）、强 `ETag`、`If-None-Match`/`If-Range`，规则同 §8 | `Range`：输出帧 k 位于字节 `44 + k × block_align` | WAV：PCM 16/24/32-bit 整数或 32/64-bit 浮点 |
| `flac`（后备） | ffmpeg 实时转码 | chunked，无总长度，不支持 Range | `t=<秒>` | 带宽低于约 2 Mbps，或 `pcm` 不支持的 WAV 编码（ADPCM 等） |

**客户端选择**：

- **务必同时传 `sr` 和 `bits`**（例如 `?codec=pcm&sr=48000&bits=16`）：两者都省略时保持源分辨率，`codec=pcm` 单独使用会把 96 kHz/24-bit 源原样返回（96 kHz/24-bit 立体声约 4.6 Mbps，并不比 `/stream` 小）。`ch` 同理，省略 = 源声道数。
- WAV：先用 `codec=pcm`（48 kHz/16-bit 立体声 1.536 Mbps，单声道 0.768 Mbps；服务端 CPU 极低；可 Range、续传、按 `ETag` 缓存）。
- 带宽低于约 2 Mbps，或 `pcm` 返回 415 `PREVIEW_UNSUPPORTED_FORMAT`：用 `codec=flac`（48 kHz/16-bit 立体声实测约 0.2–1.0 Mbps，取决于素材，安静素材更低；单声道约一半）。
- Ogg、MP3 等非 WAV 文件：直接用 `/stream`（本身已是压缩格式），不要用预览；`pcm` 对它们返回 415。
- 源已经是所请求的格式时（如 48 kHz/16-bit 立体声）`pcm` 照样生成（采样数据逐字节等于源，代价很小），客户端也可以直接用 `/stream`。

**查询参数**（未知参数忽略；同一参数重复或格式错误 → 400 `VALIDATION`）：

| 参数 | 说明 |
|---|---|
| `codec` | **必填**，`pcm` 或 `flac`（大小写不敏感）。其他值（包括以后才会支持的 `opus`）→ 400 |
| `sr` | 目标采样率，十进制整数 `8000..=192000`；省略 = 源采样率。**从不升采样**。`pcm`：只做整数倍抽取，实际输出 = 源采样率 / n，n 是满足“整除源采样率且 源/n ≤ `sr`”的最小正整数（96k→48k：n=2；192k→48k：n=4；88.2k 请求 48000 → 44.1k；44.1k、48k 请求 48000 不变；96k 请求 44100 → 32k）；n 最大 96（768k→8k），需要更大 n 的源（质数等不常见采样率，如 96001 Hz 请求 48000）→ 415 `PREVIEW_UNSUPPORTED_FORMAT`，改用 `flac`、更高的 `sr` 或 `/stream`。`flac`：输出 = min(`sr`, 源)，任意值都可以（ffmpeg 重采样） |
| `bits` | `16` 或 `24`；省略时源为 ≤16-bit 整数则 16，否则 24。不会超过整数源的位深（16-bit 源请求 24 得到 16）；浮点源可输出 24 |
| `ch` | 省略 = 源声道数；`1` = 所有声道平均（立体声即 (L+R)/2）；`2` = 多于两声道的源折叠为立体声（L = 第 0、2、4… 声道的平均，R = 第 1、3、5… 声道的平均）；≥ 源声道数 = 不变（不升混）；其他值 → 400 |
| `t` | 仅 `flac`：起始位置（秒，非负十进制，如 `12.5`、`.5`；不接受符号和指数）。`pcm` 带 `t` → 400（用 `Range` 定位） |

**实际输出格式和长度**总在响应头里（钳位/取整之后的值；两种编码相同，HEAD、206、304 也带）：

| 响应头 | 含义 |
|---|---|
| `X-Preview-Sample-Rate`、`X-Preview-Bits`、`X-Preview-Channels` | 输出格式 |
| `X-Total-Frames` | 从起点（`pcm` 为文件开头，`flac` 为 `X-Start-Frame`）到结尾的输出帧数，按**输出**采样率计。`pcm` 等于 `out_frames`（也就是 `X-Preview-Frames`，精确）；`flac` = round(源总帧数 × 输出采样率 / 源采样率) − `X-Start-Frame`（WAV 用文件头的精确帧数；非 WAV 用库记录的 `duration`，未知时不发这两个头） |
| `X-Duration` | `X-Total-Frames / 输出采样率`，秒，6 位小数（如 `6.790000`），即从起点开始的输出时长 |

服务端未配置 CORS（客户端是桌面程序），所以没有 `Access-Control-Expose-Headers`；以后加 CORS 时这些 `X-*` 头和 `ETag` 都要列入。

#### `codec=pcm`

- **支持的源**：RIFF / RF64 / BW64 WAV，`WAVE_FORMAT_PCM`（16/24/32-bit；24-bit 容器里的 20-bit 等按 24 处理）、`WAVE_FORMAT_IEEE_FLOAT`（32/64-bit），以及这两种的 `WAVE_FORMAT_EXTENSIBLE`；`bext`、`iXML`、`LIST`、`JUNK` 等其他块跳过（奇数长度块按 RIFF 规则跳过填充字节；个别写入器漏写填充字节的文件也能识别）；1..=64 声道；源采样率 1..=768000 Hz。依据是**文件头**，不是扩展名或库记录。其他（ADPCM、A-law/µ-law、8-bit、WAV 里的 MP3、非 WAV 文件），以及文件头损坏（`fmt `/`ds64` 块被文件末尾截断、位深大于 `block_align` 给出的样本容器、采样率超出范围、没有 `fmt `/`data` 块）→ **415 `PREVIEW_UNSUPPORTED_FORMAT`**，`message` 给出原因并建议 `codec=flac` 或 `/stream`，例如 `Audio 7 cannot be previewed with codec=pcm (WAV codec 0x0011 (IMA ADPCM)); use codec=flac or /stream`。
- **输出**：44 字节头的 `WAVE_FORMAT_PCM` WAV（`Content-Type: audio/wav`）。`out_frames = floor(src_frames / n)`，`Content-Length = 44 + out_frames × 声道数 × bits/8`，另有响应头 `X-Preview-Frames: <out_frames>`。源 `data` 块声明的长度超出文件时按文件实际长度计（不完整的末帧忽略）；长度为 0 或 0xFFFFFFFF（未写完或流式录音的占位值）时视为一直到文件末尾。数据超过 4 GiB 时头里的大小写 0xFFFFFFFF（`Content-Length` 仍准确）。
- **抽取**：输出帧 k 对应源帧 k·n，由以源帧 k·n 为中心的 Kaiser 窗 sinc 低通 FIR（长度 48n+1，β = 7，截止 0.45 × 输出采样率）计算，文件两端之外按 0 处理；线性相位、无延迟，输出帧 k 与源时间 k·n / 源采样率对齐。通带平坦到约 0.40 × 输出采样率（48 kHz 输出约 19.4 kHz，误差 < 0.05 dB），输出奈奎斯特频率以上的成分衰减超过 60 dB（设计约 70 dB），有测试覆盖。n = 1 时不滤波。
- **量化**：24-bit 四舍五入；16-bit 加 TPDF 抖动（±1 LSB 三角分布），噪声是（输出帧号, 声道）的哈希，**不用运行时随机数**；16-bit 源在 n = 1 且不折叠声道时不加抖动（逐字节等于源）。浮点源样本在抽取滤波之前钳到 ±1（满幅），NaN 按 0 处理，所以损坏文件里的 ±Inf 削波而不会让整段滤波窗口变成静音。
- **确定性**：输出只由源文件字节和参数决定，因此**任意字节区间都与完整下载的同一段逐字节相同**（有测试）。`ETag` 为强 ETag：源文件的 `/stream` ETag + 参数 + 算法版本，形如 `"<源 ETag 内容>-pcm2-48000-16-2"`（算法改变时版本号递增，ETag 随之变化；`pcm2` 起浮点样本在滤波前钳位）。`Range`、`If-Range`、`If-None-Match`/304、416（附 `Content-Range: bytes */<总长>`）与 §8 完全相同；`Cache-Control: private, no-cache`。
- **名额**：预览专用的 `max_previews`（服务端配置，默认 8，1..=1024；`pcm` 和 `flac` 共用，与 `/stream` 的 `max_streams` **互不影响**——`/stream` 名额被下载占满时预览照常 200）。满时 503 `BUSY` + `Retry-After: 1`（与 §8 形状相同）。名额一直占用到响应体发完或客户端断开；`pcm` 没有额外的转码名额。
- 实现：文件头在阻塞线程池解析；正文由一个阻塞线程按约 64 KiB 输出块计算，经有界通道发送；客户端断开后下一次发送失败即停止并释放名额。源数据按 1 MiB 对齐块用 `pread` 读取，并在后台线程**预读后面 2 块**（96 kHz/24-bit 立体声约 3.6 秒音频），NAS 偶发的读取延迟被已缓冲的数据掩盖；输出字节不变（Range 仍与完整下载逐字节相同）。不把整个文件读入内存。

#### `codec=flac`

- **ffmpeg**：配置 `ffmpeg_path`（默认 `/opt/homebrew/bin/ffmpeg`）。不存在或不可执行 → **503 `TRANSCODER_UNAVAILABLE`**（不带 `Retry-After`，需要管理员处理；客户端改用 `pcm` 或 `/stream`），GET 和 HEAD 都检查；服务端照常启动（启动日志一条 WARN）。
- **名额**：先占一个预览名额 `max_previews`（与 `pcm` 共用，满时立即 503 `BUSY`；不占 `/stream` 名额），真正启动 ffmpeg 前再等转码名额 `max_transcodes`（默认 4，每路占约一个 CPU 核的一小部分）：满时**最多等 0.5 秒**，仍满则 **503 `BUSY` + `Retry-After: 1`**——与 `/stream` 和预览名额满时的状态码、`error_code`、`Retry-After` 完全相同，客户端同样处理。304 和 HEAD 不等转码名额（只在处理期间短暂占用预览名额）。
- **源格式**：文件头是支持的 WAV 时取自文件头（精确帧数），否则取自库记录（`sample_rate`/`bit_depth`/`channels`/`duration`）；采样率或声道数未知 → 415 `PREVIEW_UNSUPPORTED_FORMAT`。
- **`t=`**：`start_source_frame = round(t × 源采样率)`；≥ 总帧数（即 `t` ≥ 时长）→ **416 `RANGE_NOT_SATISFIABLE`**（JSON 错误体，不带 `Content-Range`）。ffmpeg 在输入端定位（`-ss` 在 `-i` 之前；PCM WAV 直接跳转到字节位置，再精确裁剪），起点**精确到样本**（测试用斜坡信号逐样本核对）。响应头：`X-Start-Time` = start_source_frame / 源采样率（秒，6 位小数，例如 `12.345677`），`X-Start-Frame` = round(start_source_frame × 输出采样率 / 源采样率)，按**输出**采样率计（采样率成整数比时精确；44.1k 等非整数比时误差小于半个输出样本）。不带 `t` 时为 `0.000000` / `0`。
- **转码参数**：`-nostdin -hide_banner -loglevel error [-ss <X-Start-Time>] -i <源> -map 0:a:0 -vn -sn -dn -map_metadata -1 -map_chapters -1 -af [pan=…,]aresample=osr=<sr>:osf=s16|s32:dither_method=triangular -c:a flac -compression_level 5 -sample_fmt s16|s32 [-bits_per_raw_sample 24] -flush_packets 1 -f flac pipe:1`。声道折叠规则同 `pcm`（ffmpeg `pan`）。压缩级别 5：生产素材上比 0–2 小约 14%，速度相同（数百倍实时），4096 样本一帧。macOS 上 ffmpeg 通过 stdin 继承服务端已打开、已做根目录和 inode 校验的那个文件描述符（`-i /dev/fd/0`，可 seek），不会自己重新打开 NAS 路径。
- **响应**：200，`Content-Type: audio/flac`，`Transfer-Encoding: chunked`，**无 `Content-Length`**，`Accept-Ranges: none`，请求里的 `Range` 被忽略（总是 200 + 从 `t` 开始的整段），`Cache-Control: private, no-cache`（以前是 `no-store`；现在可以按 `ETag` 缓存）。
- **`ETag` / 304**：弱 ETag（ffmpeg 输出不承诺逐字节稳定，且没有 Range），形如 `W/"<源 ETag 内容>-flac1-<ffmpeg 标识>-48000-16-2-<start_source_frame>"`：源文件的 `/stream` ETag + FLAC 算法版本（ffmpeg 参数变化时递增）+ ffmpeg 可执行文件标识（大小 + 修改时间，升级 ffmpeg 后变化）+ 规范化后的 `sr`/`bits`/`ch`/起始帧，任一变化 ETag 都变。`If-None-Match` 匹配（弱比较，或 `*`）→ **304**，带 `ETag`、`X-Start-*`、`X-Preview-*`、`X-Total-Frames`、`X-Duration`，无响应体，**不启动 ffmpeg、不等转码名额**。FLAC STREAMINFO 的总样本数为 0（未知），时长用库元数据。响应头在 ffmpeg 输出第一块数据后才发出，所以 ffmpeg 一启动就失败（文件损坏等）时返回 500 `AUDIO`（JSON），而不是一个坏的 200；中途 ffmpeg 非零退出时连接被中断（chunked 没有正常结尾），客户端按“响应不完整”处理。ffmpeg 的 stderr 读入 16 KiB 有界缓冲（之后丢弃，管道不会堵塞），失败时写入日志。
- **客户端断开**：立即 SIGKILL ffmpeg 并回收进程（测试中约 20 ms），名额在进程退出后释放。
- **HEAD**：返回同样的响应头（含 `ETag`、`X-Start-*`、`X-Preview-*`、`X-Total-Frames`、`X-Duration`），**不启动 ffmpeg**（仍按 `/stream` 规则打开文件、检查根，所以 404/503 与 GET 一致）。

#### 两种编码共同的规则

- 选择文件、根目录限制、404、根离线 503 `ROOT_OFFLINE` + `Retry-After: 5`（先即时重查约 2 秒）、慢速 NAS 唤醒（**无 30 秒时间上限**，不会 408）、文件在阻塞线程池中打开、打开超过 10 秒记 WARN：与 `/stream` 完全相同（§6.4）。文件缺失是 404，不会变成一个坏掉的流。
- 以后可能的扩展：进程内（纯 Rust）FLAC 编码，替代 ffmpeg 并得到确定、可 Range 的 FLAC；Opus（`kbps=`，客户端需另配解码器）；转码结果缓存。

### 8.2 专辑封面 `GET /albums/{id}/artwork` 与兼容音频端点

schema 8 使用共享专辑实体：优先按 WAV Metadata 的 `library` 归组，缺失时按已登记根下的第一层文件夹归组。图片不进入 SQLite 或快照。详见[共享专辑与 Metadata 存储](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Shared_Albums.md)。

- 新能力 `compatibility.features.album_entity_artwork >= 1` 表示支持 `GET` / `HEAD /albums/{id}/artwork`。`id` 是当前原生快照的专辑 ID，不能发送旧快照本地投影的 ID。
- 必须带单个非空 `X-AudioLib-Library-Id` 和 `X-AudioLib-Album-Namespace`。库身份或 namespace 不符、namespace 缺失/空/重复，均在读取源文件前拒绝，返回 409 `LIBRARY_CHANGED`（库身份缺失仍为 `LIBRARY_ID_REQUIRED`）。
- 从完整专辑按音频 ID 升序选最多 64 个未缺失成员，返回第一个有效内嵌封面，与选中 WAV、搜索条件和列表页无关。
- 可选 `folder_id` 为已有目录的有符号 64 位 ID，限制为同专辑、该**直属目录**内的未缺失成员；先过滤目录，再按音频 ID 升序取最多 64 个候选，不包含子目录。该目录无图时为 404，不回退专辑其他目录。两种目录参数都省略时仍保持原全专辑语义。整数格式错误/溢出/重复为 400 `VALIDATION`，目录不存在为 404。
- 对历史 `folder_id=NULL` 成员，可改用 `unassigned_directory=<源父目录 identity>`；只查同专辑、NULL folder、该直属源父目录的未缺失成员，同样在 LIMIT 前过滤。与 `folder_id` 互斥，同时出现或参数重复为 400；原始与规范化字符串均最多 4096 UTF-8 字节。允许空字符串，表示平铺相对文件名的父目录，不能当成全专辑。Windows drive/UNC 路径在所有平台上按其源语法规范化（drive 根 identity 为 `c:`），POSIX 根为 `/`，保留目录 entry 大小写和 POSIX 字面反斜杠；identity 不用于 OS 路径访问，不扫描文件系统寻找目录。
- 客户端使用目录参数前必须确认 `compatibility.features.album_folder_artwork == 1`（并具备 `album_entity_artwork == 1`）；旧服务端可能忽略未知 query，不能把旧全专辑图片当成目录结果。原生 schema 8 来源缺少此能力时明确提示升级服务端，不能降级到 `/audio` 丢失 namespace 绑定；schema 3–7 来源仍可从自有快照先按直属目录筛候选，再调用兼容 `/audio` 端点，不能发送本地投影的 album ID。
- 200 / 304 同时返回 `X-Library-Id`、`X-Album-Id`、`X-Album-Namespace`；客户端全部核对。ETag 和缓存绑定 namespace、专辑及候选成员版本。库恢复/替换后同数字 ID 不得串图。
- 带目录上下文的 200 / 304 必须返回单个 `X-Album-Directory-Identity`：canonical directory 的 UTF-8 字节逐个编码为**小写 hex**（最多 8192 ASCII 字符，空目录对应空值）。`folder_id` 模式取当前 `get_folder.path` 的规范化值，并另回单个 `X-Album-Folder-Id` 的规范十进制 ID；NULL 模式回请求的 canonical parent，不回 folder ID。客户端核对库、namespace、专辑、目录 hex 及存在时的 folder ID；缺失、重复或不匹配时拒绝图片和 304 缓存命中，同 namespace 同 folder ID 路径变更也不得把新图片写入旧目录缓存。ETag 和代表图缓存绑定实际目录 identity；全专辑与目录图片即便字节相同也不共用 ETag。
- `side`、图片预算、鉴权、并发名额、条件请求和错误处理沿用下面的音频端点规则。无有效图片为 404。

#### 兼容 `GET /audio/{id}/artwork`

此兼容端点保留按音频记录 id 读取内嵌封面的语义（ID3 `APIC`，复用 `audiolib_audio::probe_with_artwork`），不依赖专辑存储结构。客户端连接 schema 3–7 的服务器时只升级自有快照副本，再用固定的共享成员顺序调用此端点，不发送本地 album ID。**图片字节不写入主库或快照。**

- 鉴权同 §2.2。支持 `GET` 和 `HEAD`。不受 30 秒上限；打开文件、根目录限制、根离线 503 `ROOT_OFFLINE`（先即时重查约 2 秒）与 `/stream` 相同（§6.4）。
- **支持检测**：`GET /capabilities` 的 `compatibility.features.album_artwork` ≥ 1。旧服务端没有这个 feature 时客户端跳过请求，不要改去逐条探测一个不存在的端点。
- 请求必须带单个非空 `X-AudioLib-Library-Id`，绑定当前快照的库身份；缺失、空值或重复为 409 `LIBRARY_ID_REQUIRED`，与当前服务器库不符为 409 `LIBRARY_CHANGED`，读取文件前即拒绝。200 和 304 返回 `X-Library-Id`，客户端核对一致后才缓存或显示。
- 封面解码使用独立的两个并发名额，不占试听名额；已满时返回 503 `BUSY`、`Retry-After: 1`。
- 查询参数 `side` 可省略。省略时返回嵌入的原始 JPEG 或 PNG。`side` 为 1..=512 的十进制整数时，把长边缩到不超过该值（保持比例，不放大）。其他值（含 `0`）为 400 `VALIDATION`。未知参数忽略。
- 界限与桌面端封面解码相同：编码不超过 8 MiB，解码宽或高不超过 4096，解码分配不超过 64 MiB。只接受 `image/jpeg` 和 `image/png`。
- 成功：200，`Content-Type: image/jpeg` 或 `image/png`，`Content-Length`，强 `ETag`（形如 `"aa2-<源文件 ETag 内容>-<side 的十六进制>"`，`side` 省略时为 `0`），`Cache-Control: private, no-cache`。确认封面存在后，`If-None-Match` 匹配（弱比较，或 `*`）为 304；无封面时即使发送 `*` 仍为 404。
- 无封面、文件缺失、文件损坏或提取/解码失败：**404 `NOT_FOUND`**，不是 5xx。
- 可选磁盘缓存：库文件所在目录下的 `artwork/`，键含库 id、记录 id、规范路径、源文件 ETag、`side` 和算法版本。缓存写失败仍返回图片。超过约 256 MiB 时删掉最旧的缓存文件。

## 9. 波形（已实现）

`GET /audio/{id}/waveform`（也支持 `HEAD`）返回桌面端波形视图使用的同一份数据：服务端用 `audiolib_audio` 的波形算法（`CACHE_VERSION` = 4，与本地 `waveform_cache` 相同）解码文件并计算峰值。

> **可以只取一段（缩放/局部重绘）**：`?start_frame=<起始帧>&end_frame=<结束帧（不含）>` 只计算这一段源帧的波形，`buckets` 列均分这段（例如 `GET /audio/7/waveform?start_frame=480000&end_frame=960000&buckets=1024` 取 96 kHz 文件第 5–10 秒）。帧号按**源**采样率计（与 `total_frames` 相同），不是预览的输出帧；响应的 `start_frame`/`end_frame`/`total_frames` 给出实际范围。规则见下面的查询参数。

- **查询参数**（其他未知参数忽略）：
  - `buckets`：列数，默认 `1024`，必须在 `64..=8192`，否则 400 `VALIDATION`。
  - `colors`：`1`/`true`（默认）或 `0`/`false`，是否计算频谱颜色（`colors=0` 时每个峰值的 `color` 为 `null`，计算快约 10 倍）。
  - `start_frame` / `end_frame`：帧范围（`end_frame` 不含），默认整个文件；`end_frame` 超过文件末尾时截到末尾；`end_frame <= start_frame` 或截断后为空（如 `start_frame` ≥ 总帧数）→ 400 `VALIDATION`。
  - `samples_per_pixel` = ⌈(end_frame − start_frame) / buckets⌉（至少 1），所以实际列数 ≤ `buckets`（短文件可能少于 `buckets`，最后一列可能不满）。
- **响应体**（`Content-Type: application/json`）：`audiolib_audio::WaveformData` 原样（`sample_rate`、`channels`、`frames`、`samples_per_pixel`、`peaks`），再加 `start_frame`、`end_frame`、`total_frames`：

```json
{
  "sample_rate": 48000,
  "channels": 2,
  "frames": 2880000,
  "samples_per_pixel": 2813,
  "peaks": [
    [ { "min": -0.41, "max": 0.43, "rms": 0.21, "color": [255, 196, 64], "dominant_hz": 440.2, "tonality": 0.93 }, "…" ],
    [ "… 第二声道 …" ]
  ],
  "start_frame": 0,
  "end_frame": 2880000,
  "total_frames": 2880000
}
```

  `frames` = 覆盖的帧数（`end_frame − start_frame`）；`peaks[channel][column]`，每列 `min`/`max`/`rms`（−1..1）、`color`（`[r,g,b]` 或 `null`）、`dominant_hz`、`tonality`，字段含义与桌面端相同。
- **缓存**：结果存在服务端缓存目录（默认库文件旁的 `waveforms/`，配置 `waveform_cache_dir`；上限 `waveform_cache_mb`，默认 1024 MiB，超出按最近使用淘汰）。缓存键 = `library_id` + 记录 ID + 完整 canonical 源路径 + 源文件流 `ETag`（§8：大小、mtime；Unix 加 inode、ctime，Windows 加创建时间）+ 服务端键版本 2 + 算法版本 4 + `buckets` + `colors` + `start_frame` + `end_frame`。两份大小和 mtime 相同的不同 WAV 不会复用缓存；旧键版本不再命中。命中时只读取源文件 Metadata（与 `/stream` 的打开检查相同），**不读音频数据**；文件变化后自动重新计算。
- **`ETag`**：`W/"wf4-k2-<16 位十六进制>"`，由上面的缓存键计算，任一参数、库/源身份或文件版本变化都会变化；zstd 响应的 `ETag` 另带 `-zstd` 后缀。`If-None-Match` 匹配 → 304（带 `ETag`，无响应体）。`Cache-Control: private, no-cache`。
- **压缩**：`Accept-Encoding` 含 `zstd`（q ≠ 0）时返回 `Content-Encoding: zstd`（压缩副本同样缓存），`Content-Length` 为压缩后长度。所有响应都带 `Vary: Accept-Encoding`。实测（60 s 立体声 48 kHz/24-bit，本地存储，release）：`buckets=1024&colors=1` 计算 140 ms，JSON 239 KiB → zstd 24 KiB；`colors=0` 15 ms，199 KiB → 15 KiB；`buckets=8192&colors=1` 1.9 MiB → 192 KiB。计算时间主要是读取 + 解码，NAS 上的长文件按其读取速度计。
- **并发**：同时最多 `max_waveforms` 个（默认 4）请求在打开/计算，其余**排队等待**（不返回 503）；客户端断开时立即让出名额，正在进行的解码在下一块（4096 帧）处取消，不写缓存。
- **根离线 / 慢速磁盘**：与 `/stream` 完全相同（§6.4）：根缓存为离线时先即时重查（约 2 秒），仍离线返回 503 `ROOT_OFFLINE` + `Retry-After: 5`；没有 30 秒时间上限，NAS 唤醒期间一直等待；打开超过 10 秒记 WARN。解码途中网络卷断开（超时类错误）同样返回 `ROOT_OFFLINE`。
- 其他错误：无此记录 / 文件缺失 / 不在根下 → 404 `NOT_FOUND`（同 §8）；无法解码 → 500 `AUDIO`；非整数 id 400 `VALIDATION`。

### 9.1 列表缩略波形批量 `POST /waveforms`（已实现）

> §9.1–§9.3 已经过客户端评审（决定见 §9.3），服务端已实现（§9.1–§9.2）。**不改数据库 schema，快照（§6.1）不变。** 客户端用 `GET /capabilities` 的 `list_waveforms`（§9.2）判断服务端是否支持；缺该字段的服务端没有本端点（404），列表不显示缩略波形（不要在列表滚动时改为逐条请求 `GET /audio/{id}/waveform`）。

目的：结果列表每行显示一条很小的单声道波形（128 列），一页（UI 每页 200 条，§5）一次请求取回；命中缓存时不读音频数据，未命中时不阻塞列表。

**请求**：`POST /api/v1/waveforms`，`Content-Type: application/json`（§2），按 §2.2 鉴权。只读：不受 `manage_mode` 限制，不要求 `X-AudioLib-Library-Id`，不改 `revision`。

```json
{
  "ids": [101, 102, 103, 104, 105, 106],
  "buckets": 128,
  "known": { "101": "W/\"wf4-k2-0123456789abcdef-l1\"" }
}
```

| 字段 | 说明 |
|---|---|
| `ids` | 1..=200 个正整数记录 id（与 UI 每页条数一致）；重复 id 只回答一次，按首次出现的顺序；空、超过 200 或含非正整数 → 400 `VALIDATION` |
| `buckets` | 可省略，默认 `128`。**目前只接受 `128`**，其他值 400 `VALIDATION`（`buckets must be 128 for list waveforms`）：固定列数让每条记录在缓存里只有一份、预生成（§9.2）的容量可预估。以后支持其他列数时会列在 `/capabilities` 的 `list_waveforms.buckets` 里 |
| `known` | 可省略。对象：键为 id 的十进制字符串，值为客户端已缓存的该项 `etag`（原样回传，客户端不解析）；只对 `ids` 中的 id 生效，其余忽略；键不是正整数或值不是字符串 → 400 `VALIDATION`。用对象而不是 `items` 数组：`ids` 与 `POST /audio/remove` 同形，没有本地缓存的客户端完全不用带 `known` |

**响应** 200：只要请求本身合法就是 200，单项结果放在各项的 `status` 中（不会因为某个根离线、某个文件缺失而整体 503 / 404）。用对象包一层而不是裸数组，是为了同时给出 `library_id` 和 `retry_after`：

```json
{
  "library_id": "…",
  "buckets": 128,
  "retry_after": 2,
  "items": [
    { "id": 101, "status": "not_modified", "etag": "W/\"wf4-k2-0123456789abcdef-l1\"" },
    { "id": 102, "status": "ok", "etag": "W/\"wf4-k2-fedcba9876543210-l1\"", "n": 128, "peaks": "<base64，512 字节>" },
    { "id": 103, "status": "pending" },
    { "id": 104, "status": "root_offline", "root": "SFX_Main" },
    { "id": 105, "status": "missing" },
    { "id": 106, "status": "error", "error_code": "AUDIO", "message": "Cannot decode …" }
  ]
}
```

| `status` | 含义 | 其他字段 | 客户端处理 |
|---|---|---|---|
| `ok` | 缓存中有与文件**当前版本**一致的结果 | `etag`、`n`、`peaks` | 显示，按 `etag` 存入本地缓存 |
| `not_modified` | 当前 `etag` 与 `known` 中给出的相同 | `etag` | 继续用本地副本 |
| `pending` | 还没有结果；或缓存是文件旧版本的（文件 ETag 已变）；或本次请求的时间预算内没读到文件 Metadata。已放入后台按需队列 | — | 先显示占位（有旧副本可继续显示旧副本），`retry_after` 秒后**只对仍可见的 pending id** 再请求 |
| `missing` | 无此记录、记录已标 missing，或文件在可达的根下不存在（相当于 §9 的 404） | — | 不显示，不重试（下次同步后再说） |
| `root_offline` | 该记录所在根离线：§6.4 根状态缓存为 `offline`，或读取 Metadata 时遇到网络卷错误 | `root`（根名） | 显示离线占位（可继续显示旧副本）；`/roots` 显示该根恢复后再请求 |
| `error` | 无法解码等（相当于 §9 的 500 `AUDIO` / `IO`）。失败按文件版本记在服务端内存中，文件不变不会反复重算 | `error_code`、`message`（不含路径） | 显示无波形占位，不自动重试 |

- `library_id`：处理该请求的库身份；与客户端已打开快照的 `library_id` 不同（库已被替换）时丢弃整个响应，按 §4 同步。
- `retry_after`：有 `pending` 项时为建议的重试间隔（整数秒，1..=5：按服务端实测的按需队列处理速度和本批最靠后的 `pending` 项在队列中的位置估计；本次请求的 Metadata 预算用完时还要等后台未完成的 `stat`），没有时为 `null`。它只是“下一次值得再问”的时间，不是全部完成的时间：一批 200 个冷文件通常要重试几轮才全部 `ok`。只放在响应体里，不用 `Retry-After` 头（200 响应）。

**`peaks` 格式**（每项约 512 字节）：

- 单声道：服务端对**整个文件**调用 `audiolib_audio::waveform_with_cancel`，`include_colors = false`（即 §9 的 `colors=0`），`start_frame = 0`，`samples_per_pixel = ⌈frames / 128⌉`（至少 1）。该函数按声道各给一组列（`peaks[channel][column]`，每列 `min`/`max`/`rms`，−1..1；各声道列数相同，最后一列可能不满；`colors=0` 时 `color` 为 `null`、`dominant_hz` / `tonality` 为 0）。缩略图只取 `min`/`max`：第 `i` 列 `min = min(各声道 peaks[c][i].min)`、`max = max(各声道 peaks[c][i].max)`，`rms` 丢弃。所以多声道的结果是各声道包络的并集（不是混音后的峰值），用于看出“哪里有声音”。
- 列数 `n` = ⌈frames / samples_per_pixel⌉ ≤ 128：短文件或帧数不能整除时可能少于 128（例如 1000 帧 → `samples_per_pixel` 8 → 125 列）。客户端按 `n` 横向拉伸，**不要假设恰好 128 列**。0 帧的文件为 `ok`、`n: 0`、`peaks: ""`。
- 编码：每个值先截到 −1..1（浮点 WAV 可能超出），再 `round(v × 32767)` 存为 int16；`peaks` = 按列交错 `[min₀, max₀, min₁, max₁, …]` 的 int16 **小端**字节串的标准 base64（RFC 4648，带 `=` 填充），共 `n × 4` 字节（`n = 128` 时 512 字节，base64 后 684 个字符）。
- 为什么这样编码：§9 的对象数组每列约 100 字节；JSON 整数数组每项约 1.2–1.5 KB 且要解析几百个数字；base64 int16 每项固定约 0.7 KB，解码只是一次 base64。int16（约 90 dB）对缩略图足够，以后按 dB 绘制安静素材也不会出现 int8 的台阶。200 项的响应未压缩约 150 KB。
- 压缩：`Accept-Encoding` 含 `zstd`（q ≠ 0）时 `Content-Encoding: zstd`，规则同 §9；响应都带 `Vary: Accept-Encoding`、`Cache-Control: no-store`（每次 id 组合不同，整个响应没有 `ETag`，也不支持 `If-None-Match`；逐项的 `known` 代替它）。

**`etag`**：`W/"wf4-k2-<16 位十六进制>-l1"`。十六进制部分与 `GET /audio/{id}/waveform?buckets=128&colors=0`（整个文件）的缓存键哈希**相同**，即由同样的输入计算：`library_id` + 记录 ID + 完整 canonical 源路径 + 源文件流 `ETag`（§8）+ 服务端键版本 2 + 算法版本 `CACHE_VERSION` 4 + `buckets` 128 + `colors=0` + 整文件范围；`-l1` 是列表编码版本（单声道 min/max + int16 base64，第 1 版；以后改编码就变成 `-l2`）。任一输入变化 `etag` 都变，客户端把它当作不透明字符串，只用于回传 `known`。

**服务端处理**：

1. 校验请求；按 `ids` 分块查库，得到记录的路径和 missing 标记（不访问根目录）。
2. 先看根状态缓存（§6.4）：根为 `offline` 的项直接 `root_offline`，**不访问磁盘**，也不像 §9 那样先做 2 秒即时重查（避免一页里每个离线根都要等）；需要时客户端用 `POST /roots/{name}/check`。
3. 其余项只对源文件做 `stat`（**不打开文件，不读音频数据**；比 §9 命中缓存时的打开检查更轻）得到文件 ETag 与缓存键，再查列表缓存：键一致 → `ok` / `not_modified`；没有缓存或键不一致（文件已变）→ 放入后台按需队列，返回 `pending`。
4. 时间预算：第 3 步在阻塞线程池中最多 8 个并行，整个请求的 `stat` 预算为 **2 秒**；到时仍没结果的项返回 `pending`（多半是 NAS 在唤醒），未结束的 `stat` 在后台结束，计入 §6.4 每根未结束打开的上限（超出上限的项直接 `pending`）。
5. 本端点**从不在请求内解码音频**：不占 `max_waveforms` 名额，也不使用 `max_streams` / `max_previews` / `max_transcodes` 名额；解码只在后台工作线程里进行（§9.2）。根在线且命中缓存时应在几十毫秒内返回。

**并发与错误**：同时最多 4 个 `POST /waveforms` 在处理（服务端内部常量，不是配置项），超出返回 503 `BUSY` + `Retry-After: 1`（形状同音频流 `BUSY`）。受 §2.1 通用 30 秒上限（不在豁免列表里；有第 4 步的 2 秒预算，正常碰不到 408）。整体错误只有：400 `VALIDATION`、401 `UNAUTHORIZED`、403 `ORIGIN_FORBIDDEN`（浏览器跨源）、413 `PAYLOAD_TOO_LARGE`、415 `JSON_CONTENT_TYPE_REQUIRED`、503 `UNAVAILABLE`（库未打开，§6.2）、503 `BUSY`；不会整体返回 `ROOT_OFFLINE` 或 404。

**后台按需队列**：无论是否开启预生成（§9.2）都存在。`pending` 的项进入内存中的按需队列（按 id 去重，最新请求的排在前面，使当前可见的一页先算；最多 10,000 项，超出时丢弃最旧的，客户端重试会重新入队），由 §9.2 的工作线程优先于全库预生成处理（让路规则见 §9.2：只在试听开始后的前 3 秒让路，试听期间最多 1 个解码名额；不因扫描暂停），算完写入缓存，客户端下一次请求得到 `ok`。队列不落盘，重启后由客户端重试重新建立。

**缓存**：

- 位置：沿用 `waveform_cache_dir`（默认库文件旁的 `waveforms/`）下的子目录 `list/<library_id 的哈希>/`。§9 的详情缓存是目录顶层的 `*.json` / `*.json.zst`，它的 LRU 清理只扫描顶层文件，不会碰到列表缓存。
- 键：即上面 `etag` 中的哈希（§9 的缓存键，`buckets` 128、`colors=0`、整文件）+ 列表编码版本 1。命中只需要 `stat`，**不读音频数据**。
- 每条记录只保留**一份**（文件当前版本）：文件变化后新结果覆盖旧结果。
- 分片存储：90 万个 512 字节的小文件在 APFS 上每个至少占一个 4 KiB 块（合计约 3.5 GiB），目录也过大，所以按 `id / 1024` 分片，每片一个文件（≤ 1024 条，约 0.5 MiB），每条记录 id、键哈希、`n` 和峰值。分片格式是服务端内部细节，客户端不得读取。写入：同一分片的更新在内存中合并（攒够 64 条或 2 秒），写入同目录临时文件后 `rename` **原子替换**；崩溃最多丢掉尚未落盘的结果（之后重算），不会留下半个分片。读取：最近使用的分片在内存中缓存（上限 64 MiB）。
- 容量估算：每条 512 B 峰值 + 约 24 B 记录头；909,342 条 × 512 B ≈ 4.66 亿字节 ≈ 444 MiB，约 450 MB 量级（含记录头约 465 MiB）。
- 预算：列表缓存是“每条记录一份”的完整集合，**不与 §9 的详情缓存共用 LRU**：放进同一个默认 1024 MiB 里，满库约 450 MiB 会把详情缓存挤到不足一半，而且 LRU 会让滚动不到的缩略图和详情波形来回互相淘汰。因此使用单独的子预算 `list_waveform_cache_mb`（默认 640，16..=1048576）；`waveform_cache_mb`（默认 1024）语义不变，仍只限制 §9 的详情缓存，两者之和是波形缓存目录的最大占用（默认 1664 MiB）。列表缓存超出子预算时按分片最近访问时间淘汰整片，记一条 WARN，并在 `/waveforms/status` 中报 `over_budget: true`；百万级库开启预生成前，子预算应 ≥ 记录数 × 0.55 KB。
- 删除记录：扫描结束（含取消前已提交的部分）以及 `POST /audio/remove`、`DELETE /folders/{id}`、`DELETE /roots/{name}` 之后，服务端在后台把分片中已不在库里的 id 删掉（按分片重写，原子替换）；`library_id` 变化（库被替换）时整个 `list/<旧库>` 目录删除。`PATCH /roots/{name}` 改路径后 canonical 路径变了，旧条目的键不再匹配，下次按需请求或预生成时重算覆盖。
- 文件被直接修改而没有重新扫描：请求时 `stat` 得到的文件 ETag 与缓存键不一致 → `pending` 并重算（惰性检测）。**没有定时巡检，也没有定时重扫。**

### 9.2 预生成开关 `pregenerate_list_waveforms`（已实现）

服务端设置 `pregenerate_list_waveforms`（bool，默认 `false`）。来源规则与 `manage_mode` 相同（§2.3、§6.9）：`config.toml` 给默认值，管理页或下面的 `PATCH /settings` 写入 `admin-settings.json` 覆盖（`null` = 删除覆盖、回到 `config.toml`），立即生效、无需重启。

- **关闭**（默认）：只有 `POST /waveforms` 未命中的项进入按需队列；服务端不主动读取没人请求的文件。
- **开启**：后台以最低优先级为库中所有非 missing 记录补齐缺失的列表缩略图（已有且键一致的跳过；判断键需要 `stat`，所以第一轮会对每个文件 `stat` 一次）。扫描（§6.6）发现的新增或内容变化的记录，在扫描结束后加入预生成队列。**没有每日巡检，没有定时重扫**：之后在磁盘上被直接修改的文件，只在下一次扫描或下一次被请求时（§9.1 惰性检测）重算。注意：开启后服务端会按限速读取所有文件的音频数据，补齐期间 NAS 会保持唤醒。

**后台工作线程**（按需队列与预生成共用）：

- 并发：每处理一个文件从 §9 的 **`max_waveforms` 名额池**取一个名额（与详情波形共用这个池，总解码并发仍 ≤ `max_waveforms`），同时最多持有 `list_waveform_workers` 个（默认 3，1..=64，且实际不超过 max(1, `max_waveforms` − 1)，保证详情波形至少还有一个名额）；每个文件处理完立即归还，所以详情请求最多等一个文件。从不使用 `max_streams` / `max_previews` / `max_transcodes` 名额。
- 限速：预生成每秒最多开始处理 `list_waveform_rate` 个文件（默认 10，1..=1000；91 万条约 25 小时）。按需队列不受此限速（数量由客户端可见行决定），只受并发限制（试听期间另见下面的让路规则）。
- 让路：“有进行中的试听”指有进行中的 `/audio/{id}/stream` 或 `/audio/{id}/preview`（只看 `max_streams` / `max_previews` 名额是否有被占用，不占用这些名额）。
  - **预生成**：本服务端有扫描在运行或暂停中（§6.6），或有进行中的试听（**整个持续期间**）。任一条件成立时预生成不再取新文件（正在处理的那个完成后停下），条件消失 5 秒后继续。根离线的记录跳过，根恢复 `online` 后再处理。
  - **按需队列**：只在每个 `/stream` / `/preview` 请求开始后的**前 3 秒**（客户端的预缓冲窗口，§11；seek 发起的新请求同样算）不取新文件，3 秒后继续；只要还有进行中的试听，按需队列**最多同时用 1 个解码名额**（不论 `list_waveform_workers`），试听全部结束后恢复正常并发。按需队列不因扫描暂停（只是当前可见的少量文件）。
  - 已开始的解码不会因让路而中断，只是不再取新文件。
- 手动暂停：`POST /waveforms/pregeneration/pause` / `POST /waveforms/pregeneration/resume`（无请求体，受 `manage_mode` 限制，不要求 `X-AudioLib-Library-Id`），只暂停当前进程的预生成，不改设置，重启后按设置继续；不影响按需队列。回复同 `GET /waveforms/status`。
- 解码失败的文件记为 `failed`，文件版本不变不会重试。

**`GET /api/v1/waveforms/status`**（按 §2.2 鉴权，不受 `manage_mode` 限制，30 秒超时；进度在内存中，重启后从头统计）：

```json
{
  "enabled": true,
  "state": "yielding",
  "yield_reason": "audio",
  "total": 909342,
  "cached": 412000,
  "remaining": 497342,
  "queued_on_demand": 0,
  "failed": 12,
  "skipped_offline": 0,
  "rate": 10,
  "cache_bytes": 221000000,
  "cache_limit_bytes": 671088640,
  "over_budget": false,
  "pass_started_at": 1700000000,
  "pass_finished_at": null
}
```

| 字段 | 含义 |
|---|---|
| `enabled` | 当前生效的 `pregenerate_list_waveforms` |
| `state` | `off`（未开启且按需队列为空）\| `idle`（已补齐，或开启前的按需队列已空）\| `running` \| `yielding`（让路中，见 `yield_reason`）\| `paused`（手动暂停） |
| `yield_reason` | 预生成让路的原因：`scan` \| `audio` \| `null`（按需队列在试听开始后 3 秒内的短暂让路不在这里报告） |
| `total` | 当前库中非 missing 记录数 |
| `cached` | 列表缓存中属于现有记录的条目数（按最近一次计算或校验时的键计；尚未 `stat` 的文件可能已经变化） |
| `remaining` | 本轮预生成尚未检查的记录数（未开启时为 `null`） |
| `queued_on_demand` | 按需队列长度 |
| `failed` / `skipped_offline` | 本轮解码失败 / 因根离线跳过的记录数 |
| `rate` | 生效的 `list_waveform_rate` |
| `cache_bytes` / `cache_limit_bytes` / `over_budget` | 列表缓存占用、子预算 `list_waveform_cache_mb`、是否超出 |
| `pass_started_at` / `pass_finished_at` | 本轮开始 / 完成时间（Unix 秒，未完成为 `null`） |

服务端实现说明（细节，客户端不应依赖）：`failed` 为当前已知解码失败（按文件版本记在内存中）的记录数，包括按需队列产生的失败；`over_budget` 一旦发生过分片淘汰即为 `true`，直到进程重启；`retry_after` = ⌈本批最靠后的 `pending` 项在按需队列中的位置（加上正在处理的个数）/ 实测按需吞吐⌉，限制在 1..=5 秒；Metadata 预算用完时取它与“未完成的 `stat` 数 × 本次请求实测的单个 `stat` 平均间隔”中较大者（预算内一个都没完成，或某根未结束的打开已达上限时为 5）；预算用完时未完成的 `stat` 不取消，在后台继续：所有请求共用最多 8 个并行 `stat`，同时未结束的最多 400 个（超出的项只进按需队列），正在 `stat` 的 id 不重复发起（后来的请求直接答 `pending`），每根仍受未结束打开上限约束；`stat` 结果（未命中缓存时）在内存中保留 30 秒，工作线程和后续请求直接复用，不再访问网络卷；配置项 `list_waveform_pause_stats_on_playback`（bool，默认 `false`）为 `true` 时，每个 `GET` 的 `/stream` / `/preview` 开始后的前 3 秒（与按需队列让路的预缓冲窗口相同，`HEAD` 不算）`POST /waveforms` 不做任何 `stat`：请求内不发起新的 `stat`，这些项直接答 `pending`，`retry_after` 至少覆盖窗口剩余时间（仍为 1..=5），后台继续的 `stat` 等窗口结束再做，避免与试听文件的打开争用网络卷；实测吞吐是已完成按需任务间隔的指数加权平均（新样本权重 0.2，队列空闲的时间不计），还没有完成过任务时按每个工作线程每秒 8 个估计；每个 `POST /waveforms` 在 info 级别记一行诊断日志 `List waveform batch`（id 数、查库与 `stat` 阶段毫秒数、最慢的单个 `stat`、未完成（转入后台）的 `stat` 数、因已在 `stat` 而跳过 / 因超出上限而跳过 / 因试听预缓冲暂停而跳过的数、后台未结束的 `stat` 总数、预算是否用完、各状态计数、`retry_after`、按需队列长度与实测吞吐、各根的记录数）；限速只作用于实际解码的文件，键一致只需 `stat` 的记录不计入限速；扫描结束后加入预生成的是本次扫描写入（新增或内容变化）的记录；预生成进度、按需队列和失败记录都只在内存中。“试听开始”指 `GET` 的 `/stream` / `/preview` 被接受（`HEAD` 不算，但它短暂占用名额时仍算进行中的试听）。

**客户端读取 / 修改开关**：现有的 `/admin/api/settings` 只供局域网管理页使用、客户端不得依赖（§2.3.1），`/capabilities` 是只读的，所以新增一个通用的 `/settings`，以后其他“客户端可改的服务端设置”也放在这里：

- `GET /api/v1/settings`（按 §2.2 鉴权，不受 `manage_mode` 限制；库启动中也可用；30 秒超时）→ `{"pregenerate_list_waveforms": false, "pregenerate_list_waveforms_source": "default", "can_change": true}`。`_source` 为 `default` / `config` / `admin`（同 `GET /admin/api/status`）；`can_change` 与 `/capabilities` 的 `can_manage` 相同，只是提示。
- `PATCH /api/v1/settings`，`{"pregenerate_list_waveforms": true}`：只改出现的字段，`null` = 删除覆盖、回到 `config.toml`；值类型不对或含未知字段 → 400 `VALIDATION`。**受 `manage_mode` 限制**（403 `MANAGE_FORBIDDEN`，与 §6.9 的管理端点同样在设置锁内先鉴权再检查管理权限，设置锁被占用时 409 `BUSY`）；要求 `Content-Type: application/json`，浏览器请求须同源；是服务端设置而不是库记录，所以不要求 `X-AudioLib-Library-Id`，不改 `revision`。写入 `admin-settings.json`（原子写入，同 §2.3），回复同 GET。关闭时工作线程处理完当前文件后停下，已生成的缓存保留。
- 管理页：`PUT /admin/api/settings` 同样接受 `pregenerate_list_waveforms`（`null` / `""` = 删除覆盖），`GET /admin/api/status` 增加 `pregenerate_list_waveforms` + `_source`。

**`GET /capabilities` 新增字段**（§6.9）：

```json
{ "manage_mode": "all", "external": false, "can_manage": true, "root_management": true,
  "list_waveforms": { "version": 1, "buckets": [128], "max_ids": 200, "encoding": "i16le-minmax-base64" },
  "pregenerate_list_waveforms": false }
```

- `list_waveforms`：本服务端支持 §9.1；`version` 与 `etag` 的 `-l<版本>` 一致，`encoding` 说明 `peaks` 的格式。缺字段（旧服务端）= 不支持。
- `pregenerate_list_waveforms`：当前生效的开关值（只读；修改用 `PATCH /settings`，进度看 `/waveforms/status`）。

### 9.3 本地模式与客户端决定（客户端已评审）

**本地模式**：

- 本地模式不走网络，**扫描时不同步计算**（读取每个文件的全部音频数据会明显拖慢扫描）。客户端有自己的设置「预生成列表波形」（按本地库保存，与服务端的 `pregenerate_list_waveforms` 无关），新登记本地库默认勾选，已有配置保持原选择：
  - **开启**：扫描结束后以单个最低优先级工作线程连续补齐缺失或过期的缩略图，没有每文件固定等待；每文件前检查可见行请求、扫描及播放/试听，让前台任务优先。摘要读取使用固定 2 MiB 缓冲并跳过完整 Metadata 预读，沿用相同的 128 列峰值与缓存键；
  - **关闭**：只对列表中当前可见的行按需计算。
- 算法与 §9.1 相同：`waveform_with_cancel`，`include_colors = false`，`samples_per_pixel = ⌈frames / 128⌉`，同样的跨声道 min/max 合成和 int16 量化。结果写入库中已有的 `waveform_cache` 表（**无需改 schema**）：`zoom_level = 'list128-v4-l1'`（列表 128 列 + 算法版本 + 列表编码版本，版本变化后旧行视为无效）、`samples_per_pixel`、`min_values` / `max_values` = 各 `n` 个 int16 小端（`n = 128` 时各 256 字节）、`rms_values` / `color_values` 为 `NULL`、`created_at`。不需要网络，也不使用服务端的 `waveforms/` 目录。
- 失效：磁盘上文件的大小或 mtime 与库记录不同时重算并覆盖该行。记录移除时库已经删除对应的 `waveform_cache` 行。
- 服务器模式下服务端不写 `waveform_cache` 表，快照内容不变；客户端在服务器模式下**不读取**快照里的 `waveform_cache` 行（可能是该库以前在本地模式下留下的），缩略图只来自 §9.1。

**服务器模式的客户端决定**：

1. **支持检测**：`GET /capabilities` 没有 `list_waveforms` 时客户端隐藏缩略波形列，**不**回退为逐条请求 `GET /audio/{id}/waveform`。
2. **`pending` 重试**：只对**仍可见**的行按 `retry_after`（1..=5 秒）重试，每个 id 最多重试 5 次；滚出视图的行不再请求；已发出的请求不取消（结果照常存入缓存）；每个客户端同时最多 1 个 `POST /waveforms`；503 `BUSY` 按 `Retry-After` 退避。
3. **客户端缓存**：`cache/waveforms/server/<library_id 的哈希>/list/`，分片存储，客户端私有格式；上限 640 MB，按分片淘汰；按 `etag` 失效（回传 `known`）。`pending` 和 `root_offline` 时继续显示旧副本，`root_offline` 的行变暗显示。
4. **显示**：`n < 128` 时横向拉伸填满列宽；`n = 0` 画一条中心线。
5. **设置页**：服务器模式下在「服务器音效库」页显示 `pregenerate_list_waveforms` 开关和预生成进度；`GET /settings` 的 `can_change` 为假时只读显示。只在该页打开时每 5 秒轮询一次 `GET /waveforms/status`。
6. **多声道**：各声道 min/max 包络的并集满足显示需求，不需要分声道。

**实现与上文的已知差异**（都不影响接口约定）：

- 客户端本地模式：开启「预生成列表波形」时，后台补齐除了在扫描结束后运行，**也在启动时和打开该开关时**运行；正在按需计算的本地行滚出视图时**取消**该次计算（服务器模式下已发出的 `POST /waveforms` 仍不取消，见上面第 2 条）。
- 服务端：按需队列非空时预生成也让路（按需优先）；开关关闭但按需队列还有任务时，`/waveforms/status` 的 `state` 为 `running`（不是 `off`）；`cached` 在后台清理（§9.1“删除记录”）完成前仍包含已删除记录的条目。

### 9.4 列表波形 8 位编码 `i8-minmax-base64`、`waveforms.db` 与整包下载（客户端已评审，实现中）

> 编码规格：128 列、每值 1 字节、**线性、按真实振幅绘制，不加增益字节**。只新增一种可选编码，**默认编码不变**（`i16le-minmax-base64`），已实现的 §9.1 客户端不受影响。不改主库数据库 schema、索引、FTS，快照（§6.1）不变，不影响搜索。

**命名说明**：群里讨论时叫「u8」，但 min 是负值，实际存的是**有符号** int8，所以编码名定为 `i8-minmax-base64`（避免客户端按无符号解码）。

**能力声明与请求**：

- `/capabilities` 的 `list_waveforms` 新增 `"encodings": ["i16le-minmax-base64", "i8-minmax-base64"]` 和 `"bundle": true`（见下面「整包下载」）；原 `encoding` 字段保留，仍表示默认编码。
- §9.1 请求体新增可选字段 `encoding`，默认 `"i16le-minmax-base64"`，可选 `"i8-minmax-base64"`。
- 客户端 **MUST** 只发送 `capabilities.list_waveforms.encodings` 中列出的值；`capabilities` 中**没有** `encodings` 时客户端 **MUST NOT** 发送 `encoding` 字段（旧服务端只支持 i16）。
- 未知编码（字符串，但不在 `encodings` 中）→ 400 `VALIDATION`，错误体带子码 `"sub_code": "UNSUPPORTED_ENCODING"`（`message`：`unsupported list waveform encoding "<值>"`）。`encoding` 不是字符串（如 `5`）→ 400 `VALIDATION`，**不带** `sub_code`（`message`：`encoding must be a string`）。

**响应**：顶层新增 `encoding`，**必须等于请求的编码**（未发送时为默认编码）。一次响应内所有项使用同一编码，不会混用。回显与请求不一致视为协议错误：客户端**不缓存**该响应的任何项，并按 `pending` 的退避规则（§9.3 第 2 条）退避后再试。

**量化（规范）**：

- i8 值由 §9.1 的 int16 值推导：`i8 = round(i16 × 127 / 32767)`，取整为**四舍五入、0.5 远离零**（与 Rust `f64::round` 相同；例：`16384 → 64`（63.50→64），`-16384 → -64`，`130 → 1`（0.504→1）、`129 → 0`（0.49998→0），`128 → 0`（0.496→0），`32767 → 127`，`-32767 → -127`）。客户端本地模式必须用同一方法由 int16 推导，保证本地与服务器像素一致。
- 服务端**从不输出 −128**（i16 已截到 ±32767，推导结果在 −127..127）。客户端收到 −128 时按 −127 处理。“先截到 −1..1”只适用于 f32 源值（§9.1 的 int16 量化之前），对 i8 推导不适用。
- 保证每列 `min ≤ max`（量化单调，不会翻转）。
- 解码：`v = i8 / 127.0`。
- **精度边界**：线性刻度下最小非零台阶约 1/127 ≈ **−42 dBFS**，峰值低于此的安静素材会画成一条平线。

**长度**：`peaks` 解码后**恰好 `2n` 字节**（按列交错 `[min₀, max₀, min₁, max₁, …]`），base64（RFC 4648 标准字母表，带 `=` 填充）长度为 `4 × ⌈2n / 3⌉` 字符；`n = 128` 时 256 字节、344 字符。

- `n = 0`（0 帧文件）：`status: ok`、`n: 0`、`peaks: ""`（空字符串）。
- 短文件 `n < 128`：`n` 为实际列数 = ⌈frames / samples_per_pixel⌉，`samples_per_pixel = max(1, ⌈frames / 128⌉)`；例如 1000 帧 → spp 8 → `n = 125`，50 帧 → spp 1 → `n = 50`。客户端按 `n` 横向拉伸，不得假设 128。
- 长度与 `n` 不符的项视为协议错误，不缓存。

**`etag` 与 `known`**：

- 十六进制部分与 §9.1 相同，后缀按编码区分：i16 为 `-l1`，i8 为 `-l1i8`（`W/"wf4-k2-<hex>-l1i8"`）。
- `known` 按**完整** etag 字符串（含后缀）比较。`known` 为 `-l1` 而请求 i8 时，返回 `ok` + 新的 `-l1i8` etag，**不是** `not_modified`。
- `pending` 时客户端仍显示的旧副本（`previous`）按**请求编码**对应的本地缓存取。

**客户端本地存储**：本地模式与服务器模式的 i8 列表波形都存在客户端自己的独立 `waveforms.db`（表结构同 §9.4.1），**不写入主库的 `waveform_cache` 表**；本地模式由已有 i16 结果（`waveform_cache.zoom_level = 'list128-v4-l1'`，§9.3）按上面的公式推导 i8，不重新解码音频。

**体积估算**（`n = 128`，不含 JSON 外壳与 zstd）：

| 编码 | 每条 | 90 万条 | 200 条一页（响应约） |
|---|---|---|---|
| `i16le-minmax-base64` | 512 B（base64 684 字符） | 约 461 MB | 约 150 KB |
| `i8-minmax-base64` | 256 B（base64 344 字符） | 约 230 MB | 约 82 KB |

#### 9.4.1 存储（Storage）

- 服务端把列表波形存到单独的 SQLite 文件 **`waveforms.db`**（位于 `waveform_cache_dir` 下的 `list/<library_id 的哈希>/`），**不是主库数据库**，主库 schema / FTS / 快照不变。
- 表（服务端内部，客户端经整包下载可读到，见 9.4.2）：

```sql
CREATE TABLE list_waveforms (
  file_id   INTEGER PRIMARY KEY,   -- 记录 id；普通 rowid 表（不用 WITHOUT ROWID）
  etag      TEXT    NOT NULL,      -- 完整 etag（含 -l1i8 后缀）
  src_mtime INTEGER NOT NULL,
  src_size  INTEGER NOT NULL,
  encoding  TEXT    NOT NULL,      -- 'i8-minmax-base64'
  n         INTEGER NOT NULL,      -- 实际列数 0..=128
  blob      BLOB    NOT NULL       -- 恰好 256 B：128 × (min,max) int8，n < 128 时尾部补 0
);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

- `src_mtime` / `src_size`：服务端按库记录填写，用于本机判断失效；API 和整包对客户端不承诺其含义。客户端写入**来自服务端**的条目时这两列存 `0`，失效只按 etag 判断；本地模式条目填本机文件的 mtime / size。
- 设置：`journal_mode=WAL`、`synchronous=NORMAL`；每项写入为 UPSERT（`INSERT … ON CONFLICT(file_id) DO UPDATE`），后台按批写入，每个事务几百行；事务保证不出现半行。
- 现有的 `waveforms/` 目录（§9 详情彩色波形缓存及 §9.1 的 i16 分片）**保留不变**；i8 由 i16 结果按上面的公式推导后写入 `waveforms.db`。
- 选型依据（同一组指标，90 万合成行，热缓存，p50/p95；kill 为进程被杀，**不是断电**）：

| 指标 | Windows 11 台式机（Python sqlite3 3.49.1）定长槽文件 | 同机 `waveforms.db` | Mac（Apple Silicon，外置 SSD，服务端 rusqlite（bundled SQLite），release，`examples/wfdb_bench.rs`）`waveforms.db` |
|---|---|---|---|
| 文件大小 | 252 MB | 284 MB | 308 MB（293.7 MiB） |
| 全量写入 | 0.65 s | 2.16 s（每事务 1 万行） | 1.84 s（每事务 1 万行）/ 1.87 s（每事务 256 行） |
| 单条更新 | 0.00 / 0.03 ms（fsync 0.90 ms） | 0.02 / 0.04 ms（FULL 0.90 ms） | 0.008 / 0.010 ms（FULL 0.081 / 0.153 ms） |
| 读 200 行一页 | 1.08 / 1.38 ms + 42 ms 索引加载 | 1.16 / 1.72 ms | 0.175 / 0.192 ms |
| 写入时并发读 | 不支持 | 1.06 / 1.81 ms，0 错误 | 0.178 / 0.205 ms，0 错误（同时每事务 256 行写入） |
| 随机 kill -9 | 20 次中 4 次撕裂槽（CRC 可检出，条目丢失） | integrity_check 20/20 ok | integrity_check 20/20 ok，且 20/20 次行数恰为 900,000、每行 blob 256 B（修正后的基准重跑） |

  结论：采用 `waveforms.db`、WAL、普通 `INTEGER PRIMARY KEY`、`synchronous=NORMAL`、后台批量写入（每事务几百行）。

#### 9.4.2 整包下载（Bundle）

- `GET /api/v1/waveforms/bundle?encoding=i8-minmax-base64`（按 §2.2 鉴权，只读，不受 `manage_mode` 限制；不受 §2.1 的 30 秒上限）：返回 `waveforms.db` 的**只读 SQLite 快照**（`Content-Type: application/vnd.sqlite3`；纯峰值 90 万 × 256 B ≈ 230 MB，加上 etag 等列与页开销实测约 308 MB，见下）。服务端在**单独的只读连接**上用 `VACUUM INTO` 生成临时文件（WAL 读事务，不阻塞写入和 `POST /waveforms`），再共享缓存文件流式发送，旧文件在缓存替换且所有下载释放后删除。`encoding` 目前只接受 `i8-minmax-base64`，其他值（包括 `../x` 之类，从不用作路径）→ 400 `VALIDATION` / `UNSUPPORTED_ENCODING`。`/capabilities` 的 `list_waveforms.bundle: true` 表示支持。省略 `encoding` 时按 `i8-minmax-base64` 处理（客户端应显式发送）。
- **不可用**：库已打开但服务端无法使用 `waveforms.db`（例如缓存目录无法创建）时，`/capabilities` 的 `list_waveforms.bundle` 为 `false`，本端点返回 503 `UNAVAILABLE` + `Retry-After`（不是 500）。库仍在打开时 `bundle` 照常为 `true`，本端点按 §6.2 返回 503 `UNAVAILABLE`，客户端按 `Retry-After` 重试。
- **缓存与临时文件**：整包写在 `waveforms.db` 旁的 `audiolib-wfbundle-*` 临时目录里，按库身份、主库 revision 和波形 SQLite 内容变更标识复用同一只读文件。主库 revision 未变时的波形写入也会使缓存失效。旧下载保留自己的文件引用，缓存替换不破坏在途响应；文件无人引用后删除。每个目录持有所有权锁，清理只删除带标记且未锁定的残留，不删除另一实例活动文件。
- **并发**：同一时间最多生成一个整包，准备完成即释放生成名额；缓存命中直接共享文件。同时最多 8 个下载，超限返回 503 `BUSY` + `Retry-After: 5`。客户端在准备期间断开时，准备名额仍保持到阻塞工作结束。客户端按 `Retry-After` 等待后重试，期间照常用 §9.1 按页请求。
- **超时**：本端点不受 §2.1 的 30 秒请求上限；每个整包发送最多 15 分钟，超过后中断响应并释放文件引用及下载名额，即使 HTTP 背压已停止轮询响应体。客户端按下载不完整重试。
- **响应头**：`ETag: "wfb1-<32 位十六进制>"`（整个包文件 SHA-256 的前 16 字节；同样内容同样 ETag，客户端可用于判断是否与上次导入的包相同，也可用于下面的条件请求）、`X-Library-Id`（同 `meta.library_id`）、`X-Library-Revision`（同 `meta.revision`）、`Content-Length`、`Cache-Control: private, no-cache`（允许客户端保存，但每次使用前须重新验证；旧版本为 `no-store`）。
- **ETag 接口规则**：服务端对整包**只**发送强 ETag `"wfb1-<32 位小写十六进制>"`（带双引号，**从不**发送 `W/` 弱形式）。客户端保存原值并作为字符串**精确比较**（含引号），不做大小写转换或弱比较。304 响应携带与当前整包完全相同的强 ETag（与对应 200 的 `ETag` 字节相同）。
- **条件请求（`If-None-Match`）**：客户端可发送上次导入的包的 `ETag`。服务端按 RFC 9110 弱比较判断：支持带引号的 ETag、`W/` 弱形式、逗号分隔的列表和 `*`。任一项与当前整包 ETag 相同 → `304 Not Modified`，带 `ETag`、`Cache-Control`、`X-Library-Id`、`X-Library-Revision`，**无响应体**（不打开、不发送约 308 MB 的文件）；不匹配或未发送 → 照常 `200` 全量。ETag 是包文件内容的哈希，所以缓存未命中时服务端仍须先生成整包才能比较（生成名额与 503 `BUSY` 规则同上）；缓存命中时只做比较。304 后客户端继续用本地包并按 §9.1 增量补齐。任一条目的波形变化（写入、删除、`rebuild`）都会产生新的包文件，因而得到新的 `ETag`，旧 `If-None-Match` 随之得到 `200`。客户端发送 `If-None-Match` 是可选的。旧服务端忽略该头并返回 200（客户端可比较响应 `ETag` 自行判断）。
- `meta` 表包含 `generated_at`（Unix 秒）、`library_id`、`revision`、`schema_version`、`encoding`。**`revision` 是主库的 `revision`（§4），仅供参考/诊断**：`waveforms.db` 的内容不随主库 revision 变化，也可能在同一 revision 下变化，客户端**不得**用它判断条目新旧；续传只按逐项 `etag`（下一条）。
- 客户端流程：下载到临时文件 → **校验后才使用**：`PRAGMA integrity_check` 为 `ok`；表/列名在白名单内（`list_waveforms` 与 `meta`，列同上，不多不少）；每行 `blob` 恰好 256 B、`encoding` 等于请求值、`0 ≤ n ≤ 128`；`meta.library_id` 等于当前库 → **原子替换**本地 `waveforms.db`（同目录临时文件 + rename）。任一校验失败则丢弃，不覆盖现有缓存。
- 增量（续传）：导入后用 §9.1（`known` = 包内该项的 etag）按 etag 增量拉取；打包期间或之后变化的条目 etag 已不同，会返回 `ok`，所以不会丢失。不按 `meta.revision` 或 `generated_at` 做任何过滤。
- 实测（生成时间 / 大小）：Mac 0.48 s（只读连接 `VACUUM INTO` + 元数据 + SHA-256 ETag；仅 `VACUUM INTO` 的首测为 0.38 s），308 MB（与源文件同大小），客户端式校验（integrity_check + 白名单 + blob 长度）0.23 s；Windows（待测）。

### 9.5 列表波形缓存管理：状态扩展、生成任务与限速设置（客户端已评审，服务端已实现）

> 目的：让客户端在「服务器音效库」设置页查看列表缩略波形缓存（§9.1–§9.4）的完整进度，并手动发起「补齐缺失」「全部重建」「重试失败」任务、暂停/继续/停止任务、调整生成限速。**只新增端点和字段，已有字段语义不变**；不改主库 schema，快照（§6.1）不变。客户端用 `GET /capabilities` 的 `list_waveform_jobs` 判断是否支持（缺字段 = 不支持，隐藏这些按钮，只显示 §9.2 的开关与进度）。

#### 9.5.1 任务模型

- 服务端同一时间最多一个**列表波形任务**（job）。任务与 §9.2 的「预生成」共用同一组后台工作线程、限速和让路规则：
  - 有任务在运行时，任务**取代**本轮预生成（预生成不另起一轮，任务结束后按 `pregenerate_list_waveforms` 继续）；
  - §9.1 的按需队列始终优先于任务，不受任务暂停影响；
  - 让路规则与 §9.2 的预生成完全相同（扫描运行/暂停中、试听整个期间让路，条件消失 5 秒后继续；根离线的记录跳过，根恢复后再处理）；已开始的解码不因让路中断。
- 模式 `mode`：

| `mode` | 处理范围 | 说明 |
|---|---|---|
| `missing` | 所有非 missing 记录中**没有缓存或缓存键不一致**（文件 ETag 已变，即 etag 不匹配）的项 | 已有且键一致的只做 `stat` 跳过，不计入限速（同 §9.2）。相当于“立即跑一轮预生成”，不要求开启 `pregenerate_list_waveforms` |
| `rebuild` | 所有非 missing 记录 | 先**清空**本库的列表缓存（`list/<library_id 的哈希>/` 下的 i16 分片与 `waveforms.db` 的 `list_waveforms` 行），再全部重新生成。破坏性操作：客户端须二次确认（§9.5.6）。清空期间 `POST /waveforms` 的未命中项照常返回 `pending` |
| `failed` | 当前已知解码失败（`failed`）的记录 | 清除这些记录的失败标记后重新解码；仍失败的再次记为失败 |

- **持续生效**：任务只处理**开始时**确定的记录集合（按 id 升序遍历）；任务进行中扫描新增的记录不加入本任务（开启预生成时由预生成在任务结束后处理）。任务进行中被移除的记录跳过。
- 任务结束状态：`completed`（全部处理完；有失败项也算完成，见 `failed`）、`cancelled`（被停止）、`error`（无法继续，如缓存目录不可写；见 `last_error`）。

#### 9.5.2 `GET /api/v1/waveforms/status` 新增字段（向后兼容）

已有字段（§9.2）全部保留、语义不变。新增：

```json
{
  "enabled": true,
  "state": "running",
  "yield_reason": null,
  "total": 909342,
  "cached": 412000,
  "remaining": 497342,
  "queued_on_demand": 0,
  "failed": 12,
  "skipped_offline": 0,
  "rate": 10,
  "cache_bytes": 221000000,
  "cache_limit_bytes": 671088640,
  "over_budget": false,
  "pass_started_at": 1700000000,
  "pass_finished_at": null,

  "pending": 497342,
  "run_state": "running",
  "current_rate": 9.6,
  "eta_secs": 51800,
  "last_error": null,
  "job": {
    "id": "wfj-1700000000-3f2a",
    "mode": "missing",
    "mode_effective": "missing",
    "state": "running",
    "started_at": 1700000000,
    "updated_at": 1700003600,
    "finished_at": null,
    "total": 909342,
    "done": 412000,
    "generated": 35000,
    "skipped": 377000,
    "failed": 12,
    "skipped_offline": 0,
    "resumed": false,
    "current": { "id": 412345, "started_at": 1700003599 }
  }
}
```

| 字段 | 含义 |
|---|---|
| `pending` | 尚需生成的记录数：有任务时 = `job.total − job.done`；无任务时同 `remaining`（未开启预生成时为 `null`）。新客户端用它代替 `remaining` |
| `run_state` | 归一化的运行状态：`idle` \| `running` \| `paused` \| `error`。`running` 包括已有 `state` 的 `running` 与 `yielding`（让路细节仍看 `state` / `yield_reason`）；`paused` = 任务或预生成被手动暂停（任务的暂停跨重启保留，重启后仍报 `paused`）；`error` = 最近一个任务以 `error` 结束且之后没有新任务；`off` 与 `idle` 都报 `idle` |
| `current_rate` | 最近 60 秒滑动窗口内的实际处理速度（记录/秒，一位小数）；空闲或暂停时为 `0` |
| `eta_secs` | 预计剩余秒数 = `pending` / `current_rate`（同一个 60 秒滑动窗口），向上取整；任务刚开始（窗口内尚无样本）、暂停中、`current_rate` 为 0 或 `pending` 为 `null` 时为 `null`，客户端显示「估算中」 |
| `last_error` | 最近一次后台错误（任务级错误或最近一个文件解码失败）：`{ "at": <Unix 秒>, "error_code": "AUDIO", "message": "…", "id": 106 }`（`id` 可为 `null`；`message` 不含路径，同 §9.1 的 `error`），没有时为 `null`。进程重启后从持久化任务状态恢复（§9.5.5） |
| `job` | 当前或最近一个任务；从未运行过任务时为 `null`。**必有字段**：`id`（不透明字符串，客户端用它判断任务是否已换成新的）、`mode`、`started_at`、`done`、`failed`、`total`，其余字段可能随版本增加。`mode_effective` = 实际执行的模式，通常等于 `mode`；`failed` 回退时为 `missing`（见 §9.5.5）。`state`：`running` \| `yielding` \| `paused` \| `completed` \| `cancelled` \| `error`。`done` = 已检查的记录数（`generated` + `skipped` + `failed` + `skipped_offline`）；`generated` = 实际解码写入的数；`skipped` = 键一致只做 `stat` 的数；`resumed` = 本任务是否在重启后恢复过；`current` = 正在处理的记录（没有时为 `null`）；结束的任务保留到下一个任务开始 |

当前桌面候选的速度和 ETA 使用客户端滚动估计（180 秒窗口、至少 30 秒有效运行样本，排除暂停与让路），不直接展示上述服务器 60 秒窗口估计；HTTP 字段含义保持不变。

- 已有字段 `failed`、`remaining`、`pass_*` 的含义不变；有任务时 `pass_started_at` / `pass_finished_at` 跟随任务的 `started_at` / `finished_at`。
- 仍按 §2.2 鉴权，**不受 `manage_mode` 限制**，30 秒超时。旧字段“进度在内存中，重启后从头统计”的说明改为：预生成进度仍在内存中；**任务**进度持久化（§9.5.5）。

#### 9.5.3 任务端点

所有任务端点：按 §2.2 鉴权，**受 `manage_mode` 限制**（§6.9，403 `MANAGE_FORBIDDEN`；先鉴权，缺 token 仍是 401）；浏览器请求须同源（§2）；是缓存而不是库记录，所以**不要求** `X-AudioLib-Library-Id`，不改 `revision`；30 秒超时（只登记任务，不在请求内解码）。成功时回复与 `GET /waveforms/status` 相同的完整状态（客户端无需再查一次）。

**开始**：`POST /api/v1/waveforms/jobs`，`Content-Type: application/json`

```json
{ "mode": "missing" }
```

- `mode` 必填：`missing` \| `rebuild` \| `failed`；缺失、类型不对、未知值或含未知字段 → 400 `VALIDATION`（未知 `mode` 带 `"sub_code": "UNSUPPORTED_MODE"`）。
- `rebuild` 必须同时带 `"confirm": "rebuild"`，否则 400 `VALIDATION`（`sub_code`: `CONFIRM_REQUIRED`）；防止误发。其他模式带 `confirm` 时忽略该字段。
- 成功 → **202**，状态体中 `job.id` 为新任务 id（不透明字符串）、`job.state` 为 `running`。
- 已有任务在 `running` / `yielding` / `paused` → **409 `BUSY`**（`message`：`A list waveform job is already in progress`，错误体附 `"job_id": "<当前任务 id>"`），不排队。结束状态的任务不妨碍新任务。
- 库未打开 → 503 `UNAVAILABLE`（§6.2）；列表缓存不可用（`waveforms.db` 无法使用，同 §9.4.2）→ 503 `UNAVAILABLE` + `Retry-After`。
- 扫描运行中也可以开始：任务登记后按让路规则等待扫描结束。

**暂停 / 继续**：`POST /api/v1/waveforms/jobs/current/pause`、`POST /api/v1/waveforms/jobs/current/resume`（无请求体）

- 暂停后不再取新文件（正在处理的那个完成后停下）；暂停状态会持久化，**重启后仍为暂停**（与 §9.2 预生成的手动暂停不同）。
- 幂等：对已暂停的再暂停、对运行中的再继续都返回 200。
- 没有任务、或当前任务已结束 → 409 `JOB_STATE`（新错误码，见 §9.5.7）。

**停止**：`POST /api/v1/waveforms/jobs/current/stop`（无请求体）

- 已定用 `POST …/stop`，不提供 `DELETE`：与 §6.6 扫描的 `POST /scans/{id}/cancel` 一样是“动作”，且停止后任务记录仍保留可查（`DELETE` 容易被理解成删除记录或缓存）。
- 正在处理的文件完成后停下，任务变为 `cancelled`；**已生成的缓存保留**（`rebuild` 中途停止时，已清空且尚未重建的条目之后由按需队列或下一次 `missing` 补齐）。
- 回复 200；没有进行中的任务 → 409 `JOB_STATE`。

**查询**：目前没有 `GET /waveforms/jobs/{id}`，当前/最近任务只在 `GET /waveforms/status` 的 `job` 中给出；以后如需任务历史再增加。

**与 §9.2 旧端点的关系**：`POST /waveforms/pregeneration/pause|resume` **保留**，行为扩展为：有进行中的任务时作用于任务（等同 `jobs/current/pause|resume`，但与旧行为保持一致，没有任务时也不报错）；没有任务时与原来相同，只暂停当前进程的预生成。新客户端在支持 `list_waveform_jobs` 时用新端点。

#### 9.5.4 限速设置 `list_waveform_rate`

- `GET /api/v1/settings` 新增 `list_waveform_rate`（生效值，整数，文件/秒）和 `list_waveform_rate_source`（`default` \| `config` \| `admin`）。
- `PATCH /api/v1/settings` 接受 `{"list_waveform_rate": 20}`：整数 1..=1000，越界或类型不对 → 400 `VALIDATION`；`null` = 删除覆盖、回到 `config.toml`（默认 10）。规则与 `pregenerate_list_waveforms` 完全相同（§9.2：受 `manage_mode` 限制、写入 `admin-settings.json`、立即生效），对进行中的任务和预生成**立即生效**（下一个文件起）。按需队列仍不受限速。
- `pregenerate_list_waveforms` 不变，即「新扫描到的文件自动生成缩略波形」开关；客户端设置页把两者放在一起显示。
- 管理页：`PUT /admin/api/settings` 同样接受 `list_waveform_rate`（`null` / `""` = 删除覆盖）。

#### 9.5.5 跨重启恢复：任务状态文件

- 位置：本库列表缓存目录 `list/<library_id 的哈希>/` 下的 `job-state.json`（与 `waveforms.db` 同目录）。服务端内部文件，客户端不读取。
- 写入：同目录临时文件 + `rename` 原子替换；任务开始、暂停/继续/停止、结束时立即写入，运行中最多每 **5 秒**或每 1,000 条记录写一次进度。
- 格式（`version` 1）：

```json
{
  "version": 1,
  "library_id": "…",
  "job": {
    "id": "wfj-1700000000-3f2a",
    "mode": "missing",
    "mode_effective": "missing",
    "state": "running",
    "paused": false,
    "started_at": 1700000000,
    "updated_at": 1700003600,
    "finished_at": null,
    "cursor": 412345,
    "max_id": 1203344,
    "total": 909342,
    "done": 412000,
    "generated": 35000,
    "skipped": 377000,
    "failed": 12,
    "skipped_offline": 0,
    "rebuild_cleared": true
  },
  "last_error": null
}
```

- 语义：
  - `cursor` = 已完成处理的最大记录 id（按 id 升序遍历）；恢复时从 `cursor + 1` 继续，范围上限为任务开始时的 `max_id`。由于进度最多 5 秒落盘一次，恢复后可能重复处理最后几秒的记录：`missing` 下键一致只需 `stat`，重复无害。
  - `rebuild`：`rebuild_cleared` 为 `false` 时（清空过程中被中断）恢复时重新清空；为 `true` 后按 `cursor` 继续生成。
  - 启动时（打开 `waveforms.db` 之后）若 `state` 为 `running` / `yielding` / `paused`，恢复该任务，`job.resumed` = `true`；`paused` 恢复后仍暂停。
  - `library_id` 与当前库不同、`version` 未知或文件损坏：忽略并改名为 `job-state.json.bad`，记 WARN，不恢复（不影响服务启动）。
  - 已结束的任务也保留在文件中，供 `GET /waveforms/status` 的 `job` 与 `last_error` 在重启后仍可查。
  - 失败记录的明细（哪些 id 失败）**不持久化**，只持久化计数。重启后恢复的 `mode: "failed"` 任务，以及重启后新开始、但本进程尚无已知失败项的 `failed` 任务，都**回退为 `missing`**：失败的文件没有缓存，`missing` 会覆盖它们（代价是也会补齐其他缺失项）。回退时 `job.mode` 仍为 `failed`，`job.mode_effective` 为 `missing`，客户端可据此提示「已按补齐缺失执行」。

#### 实现细节（客户端不应依赖）

- `done` 中「已删除 / 文件缺失 / 无法读取 Metadata」的记录计入 `skipped`；已知解码失败且文件未变的记录（`missing` 模式下）计入 `failed`。`current_rate` 按「已检查的记录」计（含只做 `stat` 跳过的），不含因根离线跳过的记录。
- 根离线跳过的记录先计入 `skipped_offline`；任务运行期间该根恢复 `online` 时重新处理（从 `skipped_offline` / `done` 中扣回再重新计数）。任务**不等待**离线根：其余记录处理完即 `completed`，此时仍离线的记录保留在 `skipped_offline` 中，之后由按需队列或下一次任务补齐。
- 重启后恢复的 `failed` 任务（`mode_effective` 回退为 `missing`）从头开始：`cursor`、`done` 及各计数清零，`total` 按当前库重新统计。
- 结束（`completed` / `error`）时的状态文件在最后一条记录处理完后立即写入，可能比 `GET /waveforms/status` 晚几毫秒。
- `GET /settings` 的 `list_waveform_rate_source`：`config.toml` 中的值等于内置默认 10 时报 `default`（无法区分“写了 10”与“没写”），其他值报 `config`。

#### 9.5.6 整包与客户端行为

- 任务进行中 `GET /waveforms/bundle`（§9.4.2）照常可用，返回当前已有的条目（`VACUUM INTO` 是 WAL 读事务，不阻塞任务写入）；`rebuild` 刚清空后整包可能接近空，客户端导入后用 §9.1 增量补齐即可（不会丢失：etag 规则不变）。`POST /waveforms` 照常可用。
- 建议的客户端交互：设置页显示 `run_state`、`cached` / `total`、`pending`、`failed`、`current_rate`、`eta_secs`、`last_error`、`job.current`；按钮「补齐缺失」（`missing`）、「重试失败」（`failed`，`failed > 0` 时可用）、「全部重建」（`rebuild`，**两次确认**后才发送，第二次确认提示会清空现有缓存并重新读取所有文件，NAS 会长时间保持唤醒）、「暂停 / 继续」「停止」；限速输入框（1..=1000，留空 = 恢复默认）。`can_manage` / `can_change` 为假时只读。「波形缓存」面板打开时每 **2 秒**轮询一次 `GET /waveforms/status`，面板关闭即停止轮询（状态在内存中，开销很小；与 §9.3 第 5 条预生成进度的 5 秒轮询独立）。
- 收到 409 `BUSY` 时刷新状态并显示当前任务；收到 409 `JOB_STATE` 时刷新状态即可。

#### 9.5.7 能力声明与错误码

`GET /capabilities` 新增：

```json
"list_waveform_jobs": { "version": 1, "modes": ["missing", "rebuild", "failed"], "rate_setting": true }
```

- 缺字段（旧服务端，或库已打开但 `waveforms.db`（§9.4）无法打开）= 不支持 §9.5 的任务端点与 `list_waveform_rate` 设置（后一种情况下任务端点返回 503 `UNAVAILABLE`）；`modes` 以后可扩展，客户端只显示列出的模式；`rate_setting` 表示 `PATCH /settings` 接受 `list_waveform_rate`。
- §6.9 的 `manage_mode` 管理端点清单增加：`POST /waveforms/jobs`、`POST /waveforms/jobs/current/pause|resume|stop`。
- §2.1 错误表新增：任务当前状态不允许该操作（没有任务或任务已结束时暂停/继续/停止）→ 409 `JOB_STATE`（类比 `SCAN_STATE`）。已有任务时再开始 → 409 `BUSY`（同扫描，§6.6）。

#### 9.5.8 已知问题（随本功能一起修复）

- **整包首次请求 500 `IO`（ENOENT）**：库的 `list/<library_id 的哈希>/` 目录尚未创建时，第一次 `GET /waveforms/bundle` 可能在创建目录与生成临时目录之间竞争，返回 500 `IO`（No such file or directory）。已修复：生成整包前在持有整包名额的情况下 `create_dir_all` 该目录（幂等）；目录无法创建时返回 503 `UNAVAILABLE` + `Retry-After`（`message` 不含路径），不返回 500。目录仍按需创建（第一次写入分片、`waveforms.db`、`job-state.json` 或整包时），库打开时不预先创建。旧服务端遇到该 500 时客户端可按 `Retry-After: 5` 重试一次。

## 10. 服务端部署

- 部署方式：macOS 原生进程（不用容器），默认监听 `0.0.0.0:8790`，可用 launchd 用户代理常驻。启动时先监听端口，再在后台打开库；打不开时进程不退出而是重试（§6.2）。端口被占用、配置无效或（`auth = true` 时）token 缺失仍会直接退出。可选的原生 TLS 监听（§2.3）起不来时只在日志和管理页报错，不影响 HTTP 端口。管理页：`http://<host>:8790/admin`（§2.3）。
- 配置：TOML 文件，通过 `--config <path>` 或环境变量 `AUDIOLIB_SERVER_CONFIG` 指定；字段 `bind`、`library_db`、`auth`（默认 `false`；`true` = `auth_mode = "all"`）、`auth_mode`（`off`/`external`/`all`，默认 `external`，§2.2）、`manage_mode`（`all`/`lan`/`off`，默认 `lan`，§6.9）、`public_url`（可选，§2.3）、`tls_cert_path` + `tls_key_path` + `tls_port`（可选，三项同时设置，原生 TLS，§2.3）、`token_env`、`revision_poll_secs`、`trust_lan_peers`（默认 `true`，隐藏来源的部署设 `false`）、`snapshot_dir`（默认库文件旁的 `snapshots/`）、`max_streams`（默认 16）、`root_check_secs`（默认 300）、`root_offline_secs`（默认 120）、`waveform_cache_dir`（默认库文件旁的 `waveforms/`）、`waveform_cache_mb`（默认 1024）、`max_waveforms`（默认 4）、`pregenerate_list_waveforms`（默认 `false`，列表缩略波形预生成，管理页 / `PATCH /settings` 可覆盖，§9.2）、`list_waveform_cache_mb`（默认 640，列表缩略波形缓存子预算，不计入 `waveform_cache_mb`，§9.1）、`list_waveform_workers`（默认 3，占用 `max_waveforms` 名额的上限，§9.2）、`list_waveform_rate`（默认 10，预生成每秒最多开始处理的文件数，§9.2）、`list_waveform_pause_stats_on_playback`（默认 `false`，试听开始后 3 秒内暂停 `POST /waveforms` 的 `stat`，§9.2）、`ffmpeg_path`（默认 `/opt/homebrew/bin/ffmpeg`，须为绝对路径；不存在时服务端照常启动，只有 `flac` 预览返回 503 `TRANSCODER_UNAVAILABLE`）、`max_transcodes`（默认 4，1..=64）、`max_previews`（默认 8，1..=1024，`/preview` 两种编码共用的并发上限，与 `max_streams` 分开）、`[[roots]] { name, server_path, webdav_url? }`（`server_path` 填真实路径，不经符号链接，与扫描器写入库的路径一致）。示例见 `server/audiolib-server/config.example.toml`。服务端只打开已有的库，不会创建库。
- token：环境变量 `AUDIOLIB_SERVER_TOKEN`（可用 `token_env` 改名），至少 32 个随机可见 ASCII 字符；配置文件里写 `token` 会被拒绝。`auth = true` 时必须设置；否则可选，设置了就和管理页生成的 token 一样有效（格式不对时记警告并忽略）。管理页生成的 token 和设置保存在库目录的 `tokens.json` / `admin-settings.json`（§2.3）。
- macOS 部署：launchd 可通过固定不变的 `audiolib-launcher`（`server/launcher/`，可选，只编译签名一次）用 `posix_spawn` 启动服务端，macOS 的卷访问授权记在启动器上，之后只替换服务端二进制不会再弹授权；通用部署/升级/回滚说明和 systemd、launchd、Docker 示例见 `server/deploy/`。库文件、旁路文件和 `snapshots/`（以及波形缓存 `waveforms/`）建议放在本机磁盘，例如 `/var/lib/audiolib/`（macOS：`/usr/local/var/audiolib/`）。
- 构建：在 `server/` 目录内运行 cargo（`server/rust-toolchain.toml` 与 `server/.cargo/config.toml` 覆盖仓库根的 Windows MSVC 默认设置），例如 `cargo build --release -p audiolib-server`。详见 `server/README.md`。

## 11. 客户端行为


桌面「数据源与音效库 → 服务器音效库」（仅当前服务器，且 `GET /capabilities` 的 `can_manage` 为真）：

- **开始扫描…**：选择根目录、可选子目录（相对路径）、增量或完整；并行数用服务器默认。`POST /scans`，之后每 500 ms `GET /scans/{id}` 直到 `active: false`；打开页面时先看 `GET /scans/current`。**暂停** / **继续** / **取消扫描**：`POST /scans/{id}/pause|resume|cancel`。扫描结束后若 `revision` 高于已打开的快照，按普通快照同步。
- **移除所选记录…** / **删除目录记录…**：`POST /audio/remove`（主列表勾选的 id）和 `DELETE /folders/{id}`（主窗口目录树所选目录）。确认文案写明只删除库记录、服务器上的 WAV 不改动、重新扫描会以新 id 加回。扫描进行中服务器返回 409 `BUSY`。成功后客户端丢弃按 id 缓存的选择、波形和试听，并在版本号变化后重新同步。
- `can_manage` 为假时这些按钮不可用，并显示服务器的 `manage_mode`（lan / off）说明。403 `MANAGE_FORBIDDEN`、409 `BUSY` / `SCAN_STATE`、503 `ROOT_OFFLINE`（含 `Retry-After`）按服务器原文提示。
- 主窗口原有的移除入口（编辑菜单的移除所选记录 / 移除选择记录 / 移除当前目录、结果和目录树右键菜单、列表的「移除」按钮）在服务器模式下，当前服务器 `can_manage` 为真时可用，确认文案和请求与上面相同；否则置灰并附服务器的原因。客户端每次同步后重新读取 `GET /capabilities`。
- 乐观移除：`POST /audio/remove` 或 `DELETE /folders/{id}` 返回 200 后，客户端立即在内存中隐藏这些 id（目录删除则隐藏该目录子树及其记录），从列表、目录树和记录数中去掉，并清除选择及这些 id 的试听/波形缓存；快照文件不做修改。版本号变化后的重新同步打开新快照时撤销隐藏，以快照为准。

客户端已在 `crates/audiolib-remote` + 桌面「文件 → 数据源设置…」+ `AudioLibCli remote status|test|sync|edit|plays|download` 实现，细节见 [Rust_Remote_Mode.md](../Rust_Remote_Mode.md)。要点：

- 设置：`settings.json` 新增 `data_source { mode: local|server, server_url, webdav { base_url?, username, root_urls } }`，旧设置文件按本地模式加载。token 和 WebDAV 密码不进 `settings.json`，用 Windows DPAPI 加密存于 `<数据目录>/remote/secrets.bin`，不写日志、不进导出。token 可选：留空时客户端所有请求（含快照、音频流、ETag 复核）都不带 `Authorization`，CLI `remote test`/`sync` 和桌面数据源对话框、同步、轮询、试听均可在无 token 的服务器上工作（对话框和 `remote test` 只给出提示）；此时服务器若回 401，提示「服务器要求访问令牌（401）」，而不是「令牌被拒绝」。填写的 token 对所有请求（API、快照、`/stream`、`/preview`、`/waveform`、ETag 复核）都以 `Authorization: Bearer <token>` 发送；局域网直连时服务端忽略它（§2.2），所以同一份保存的设置配上 token 既可用局域网地址也可用外网地址。
- 地址：对话框可填「完整地址」（如 `https://audiolib.example.com:8443`、`http://192.0.2.10:8790`，可带路径前缀 `https://example.com/audiolib`），或切到「域名 + 端口」分别填协议（默认 `https://`）、域名或 IP、端口（留空 = 443/80）和可选路径前缀。输入去首尾空格后规范化为基础地址 `协议://主机[:端口][/前缀]` 存入 `server_url`（`settings.json` 结构不变）：不写协议时默认 `https://`，协议小写，去掉结尾 `/`，粘贴的 `/api/v1` 去掉后由客户端统一补一次（不会重复）；主机按 DNS 名（每段 1–63 个字母数字/连字符/下划线，总长 ≤ 253）、IPv4 或方括号 IPv6 校验，端口 1–65535，前缀不能含 `.`/`..` 段、空格、`?`、`#`、用户名密码或非 ASCII；错误给出中英文原因。旧设置 `http://192.0.2.10:8790` 原样有效（API 地址与缓存键不变，无需迁移）。对话框实时显示将连接的 API 地址。容忍响应里的多余字段，`/roots` 也接受 `path` 别名，`available`/`reachable` 视同 `online`。
- HTTPS：所有请求（API、快照、音频流/预览/波形、WebDAV）共用同一 keep-alive 连接池和同一 TLS 配置（见下文「连接复用」）：rustls（ring）+ `rustls-platform-verifier`，用**操作系统信任库**校验证书链和主机名（Windows 证书存储，含企业根证书和按需更新的根）。只接受公共受信任的证书，**不接受自签名证书，也没有跳过校验的选项**。证书错误分类提示：不受信任（提示服务器需使用公共受信任的证书，如 Let's Encrypt）、主机名不匹配（证书未覆盖该域名）、已过期/尚未生效（并提示检查本机时间）、已吊销、握手失败（提示该端口可能只支持 http://）；连接被拒绝、DNS 解析失败、超时也分别提示。
- 401：缺失和错误的 token 都是 401 `UNAUTHORIZED`（§2.2），客户端按「本次请求是否带了 token」区分「服务器要求访问令牌」与「令牌错误或已被撤销」，不匹配 `message`；两种提示都引导用户在局域网内打开服务器网页管理页面 `http://<服务器局域网地址>:8790/admin` 生成令牌，再填入「数据源设置 → 访问令牌」。客户端从不请求 `/admin`。
- 连接测试步骤：DNS / TCP 连接（解析结果与耗时）→ TLS 握手 / 证书（https：协议版本、密码套件、证书主体、颁发者、到期时间、信任库；http：注明未加密）→ 服务器在线（`/health`，免鉴权）→ 令牌检查 / 素材库（`/library`）→ 根目录 → 音频流采样 → 试听预览吞吐（`GET /audio/{id}/preview?codec=pcm&sr=48000&bits=16`，`Range: bytes=0-524287`，报告首字节时间与 Mbit/s，可选项）→ 各根 WebDAV。每步给出成功/失败及原因；网络步骤失败时其余步骤跳过。`/health` 带 `public_url` 且与所测地址不同时（如在局域网测试），显示「服务器外网地址：…」，对话框提供「使用服务器外网地址」一键填入。`AudioLibCli remote test --url <地址>` / `remote status --url <地址>` 可临时指定地址（不保存）；`remote test` 的 JSON 含 `server_url`、`tls`（握手详情）和 `public_url`，`remote status` 给出解析后的地址和证书校验策略。
- 同步与准入：启动（服务器模式）、「切换到此数据源」（或保存当前服务器配置的连接改动）、「立即同步」、F5 刷新时执行。先 `/health`（免鉴权）→ `/capabilities` → `/library` → `/roots`，按[当前兼容目录与标记检查实现](../../crates/audiolib-remote/src/compatibility.rs)检查对端准入，再检查精确快照格式、完整 feature 和实际文件；不能只凭程序版本或 `user_version` 上限判断兼容。schema 6 快照仍可按目录中声明的格式只读使用，不因本机构建的写入 schema 较新就改写服务器原库。已有匹配身份、revision 和格式的可用缓存，且没有待完成的传输时，通常只更新连接信息与检查时间；`--force` 强制同步。
- 流式传输与双进度：请求 `/library/snapshot` 时带 `Accept-Encoding: zstd`，以 256 KiB 缓冲同时保存实际 HTTP 表示并解码到自有暂存数据库，不把整份快照放入内存。连接页、缓存/导出页分别显示下载和解压文字及进度条：下载累计实际传输字节，压缩时总量取 `Content-Length`；解压累计成功写入数据库的字节，总量取 `X-Uncompressed-Length`，预分配文件长度不算已解压量。两者重叠进行，下载到 100% 后解压仍可继续；任一总量未知时仅对应进度条保持不定，未压缩响应只显示下载。等待响应头时显示服务器准备状态，校验与派生准备阶段另行显示。
- 持久化续传：`<数据目录>/remote/.transfers/<规范化 API 地址的 SHA256>/` 保存原始传输表示 `payload.part` 和原子 `checkpoint.json`；checkpoint 记录对象身份、编码、长度、已落盘偏移及前缀 SHA256，不含凭据。同端点 OS lease 排除另一个 GUI/CLI 写者。每新增 8 MiB 或距上次提交达到 2 秒时，在读写循环中刷新 checkpoint，正常暂停/中断时也提交。恢复按 checkpoint 偏移截掉未提交尾部并校验前缀 SHA256，再重放该前缀重建解码器，按 §6.1 用同一 `snapshot_id`、`Range: bytes=<偏移>-` 和强 `If-Range` 请求后缀；只有 `206` 的 Content-Range、ETag、库身份、revision、格式、编码和长度均匹配才续接。zstd 后缀不能单独解压，重放不计入网络吞吐；完整 wire 文件可只做本地解码。续传收到完整 `200`、`410`、`416` 或表示变化时丢弃旧传输并安全重下，不能混接；缺少续传头的兼容服务端从零重试。
- 暂停与恢复：GUI 提供暂停/继续及重新下载；手动暂停意图跨重启保留，启动、轮询和写入后的自动同步不会解除暂停。关闭会取消活动连接、保存 checkpoint 并排空 worker；下次启动可继续未手动暂停的中断任务。`AudioLibCli remote sync` 可复用已保存的前缀，`--restart` 明确丢弃待完成传输并从零开始。暂停或失败时仍可使用匹配的已验证旧缓存；首次同步尚无缓存则显示占位空库，未完成数据库不会被打开为音效库。
- 校验与发布：检查传输长度、`X-Library-Id`、固定表示的 revision、响应声明格式及实际 SQLite `user_version`/feature；恢复固定旧表示时以其已保存身份和格式核对，而不是混用最新 `/library` 的格式。再在自有暂存文件执行 `quick_check`、外键、FTS5 integrity-check、触发器和只读打开检查，切为 `journal_mode=DELETE`；必要派生修复只发生在自有 staging/copy。通过后持短期发布 lease，拒绝用落后于当前缓存 revision 的快照覆盖新状态，再原子改名、记录缓存并切换 provider；切换前排空 Metadata 写入并失效旧 reader/UI generation。传输文件保留到对应快照已发布且 provider 成功打开后，按精确发布文件回执清理；失败保留原缓存。
- 超时与重试：快照响应头超时 240 秒，正文每次等待网络数据超过 **90 秒**视为可重试超时；没有正文总计 1 小时的限制，本地前缀重放、解压和磁盘工作不计入网络停滞阈值。断线/超时最多自动重试 3 次，等待 2、5、10 秒，兼容时从 checkpoint 续传。`409/503 BUSY` 在首次请求起计的 240 秒窗口内重试，优先使用 `Retry-After`（单次等待最多 10 秒），否则采用指数退避；响应处理完成时判断窗口是否到期，该窗口不是整次同步的绝对期限。`507 INSUFFICIENT_STORAGE` 不重试。
- 缓存：`<数据目录>/remote/<library_id>/` 保存唯一命名的 `library-r<revision>-<pid>-<纳秒>.db`（自有准备副本可不带 pid）和 `cache.json`；Windows 不替换仍被打开的固定 SQLite 文件。旧快照和同服务器被替换库的生成文件只在 provider 成功切换后清理，当前打开的及 `cache.json` 记录的快照受保护，仍被占用时稍后重试。启动和切换时清理超过 1 小时的已知顶层暂存文件，需取得 staging/publication lease；未知文件、链接、手动备份、受保护恢复任务和 `.transfers/` 续传文件保留。同端点库身份更换或固定表示失效可丢弃该端点旧传输，其他服务器缓存保留。旧版本生成的 `audio-cache/` 文件按受控命名空间清理；拖出缓存 `<数据目录>/cache/<library_id>/` 跟随全局 4 GiB LRU 淘汰。服务器不可达时打开上次匹配缓存，从未同步过则显示空库并提示。
- `library_id` 变化：提示警告，清空选择与草稿，重新下载快照。401 提示令牌无效；连接失败、超时（连接 3 秒）、`INTERNAL` 等均有明确中英文提示，不会静默显示空库。服务端 408 `TIMEOUT` 按超时处理（可重试，轮询显示离线并继续用缓存），不当作令牌或协议错误。
- 版本轮询：服务器模式下后台线程每 `poll_interval_secs` 秒（默认 60，0 = 关闭，设置项 `data_source.poll_interval_secs`）调用 `GET /library`，与已打开快照的 `library_id`/`revision` 比对；有变化时在标题栏下方显示非阻塞提示和「同步」按钮（`auto_sync = true` 时自动同步，但要等 revision 稳定：同一 revision 至少保持一个轮询间隔才同步（「重试」、保存设置等额外检查不算），持续变化时最多推迟 15 分钟，避免分批扫描时每批都整库下载；`library_id` 变化或首次同步立即进行）。轮询能连上服务器时同时取 `GET /roots`（§6.4 状态缓存），标题栏显示「根目录离线：X」「磁盘唤醒中：Y」，数据源对话框按根显示状态；离线根的记录照常列出，不标为缺失。根离线提示上的「重试」对每个离线根调用 `POST /roots/{name}/check`（约 2 秒，不阻塞界面），用返回的条目更新状态，仍离线则保留提示；服务器没有该端点（404）时改为对所选记录发 `HEAD /audio/{id}/stream`。切到本地模式、改用离线缓存或保存设置时丢弃旧的根状态（保存设置后立即重新获取，成功同步会带来新的根状态）；只有快照中保存的根列表时不使用其中过时的状态。离线时继续用缓存并显示「离线」，按间隔翻倍退避（最长 15 分钟）；`503` 视为「服务器启动中，稍后重试」（10/20/40 秒…重试，且不早于 `Retry-After`，不当作令牌错误；`/health` 的 `status: starting|error` 在连接测试中单独显示）；401 提示检查令牌。
- 标注写入（§6.3）：服务器模式下详情面板的收藏、标签、备注，以及右键/菜单的收藏、批量标注均可用，经 `DataProvider`（`audiolib_remote::RemoteProvider`）发到服务器；颜色含清除可通过 CLI 标注，标签支持替换/追加/移除。单条 `PATCH /audio/{id}/metadata`，批量 `POST /audio/metadata/batch {ids, patch, tag_mode}`；0.4.0 不提供评分编辑。**revision 规则**：成功后若本地快照 revision 等于 `revision_before`，把返回的 `file`/`files` 在一个事务中写入已打开的快照（用户标签/备注、收藏、颜色；按实际快照格式维护派生搜索与 FTS5），并把本地 revision 设为 `revision`（`cache.json` 与标题栏），不做整库同步；否则（期间有其他客户端写入、无缓存状态等）执行一次完整快照同步。错误映射：`400 VALIDATION` → 值无效（附服务器说明；发送前也做本地校验）；`404` → 记录在服务器上已不存在（批量整体回滚）；`409 BUSY` → 服务器正在扫描，稍后重试；`413 PAYLOAD_TOO_LARGE` → 请求过大（8 MiB 上限，客户端发送前也拦截）；`401` 同读取；`408 TIMEOUT`/超时/断线/响应不可读 → 结果不确定，提示错误并请求完整同步。写入不自动重试；保存失败时草稿保留为「未保存」。快照提供者切换必须先排空 Metadata 写入，排空失败则保留当前提供者与草稿，不安装新快照。
- 仍只读：扫描/重扫（主窗口入口；扫描在「服务器音效库」页）、维护/还原、relink、导入、新建/打开库、在资源管理器中定位。菜单项置灰并带「（服务器模式只读）」后缀，音效库管理对话框有说明行，控制器（`refuse_remote_write`）和 `RemoteProvider` 都独立拒绝。移除记录或目录见上（需服务器 `can_manage`）。原始 WAV 从不修改。
- 播放与本机历史（本 PR 候选）：主库不再累计播放次数；本地和服务器模式的成功播放会话只写入独立的本机 `history.db`（默认最多 5,000 条，可设为 0 禁用）。客户端不请求 `/audio/{id}/plays`，不因播放修改服务端 revision 或快照，也不触发重同步；低层兼容空操作只服务旧调用方。本地旧事件在受保护的 v6 升级中先从已验证备份导入本机历史，再退役主库播放结构；服务端升级不导入本机历史。升级前完整备份保留旧值用于回滚（§6.3、[迁移契约 §7](Library_Migration_Compatibility.md#7-通用结构演进与播放统计退役)）。
- 拖出：结果行可拖到资源管理器/DAW（拖动已勾选的行则拖出全部勾选项），原生 OLE `DoDragDrop` + `CF_HDROP` 真实路径，仅 `DROPEFFECT_COPY`；不使用虚拟/延迟文件。本地模式直接提供素材库文件。服务器模式先确保所选文件完整位于下载缓存 `<数据目录>/cache/<library_id>/<file_id>-<ETag 哈希 16 位>/<原文件名>`（状态栏显示进度，可取消；若松开鼠标前未就绪，提示「已准备好，请再次拖动」）。文件名取 §8 `download=1` 的 `Content-Disposition`：优先 `filename*=UTF-8''…`（百分号解码），否则 `filename`，再否则记录文件名；替换 Windows 非法字符、处理保留名（CON/NUL/COM1…）和末尾点/空格，例如 `filename="audio-7.wav"; filename*=UTF-8''%E9%9B%B7%E5%A3%B0%20%E8%BF%91.wav` → `雷声 近.wav`。复用：已有完整文件先 `HEAD …?download=1` 复核 ETag（最多等 2.5 秒；超时、根离线、服务器启动中或忙时直接用该缓存文件，不等磁盘唤醒）；试听内存中已有完整且 ETag（同一 `/audio/{id}/stream`）和长度一致的原始副本则直接复制；否则 `GET …/stream?download=1` 写 `.part`（`Content-Length` 驱动进度，断点用 `Range` + `If-Range` 续传，校验 ETag/长度后改名）。离线时用该文件最新的缓存版本。容量上限 4 GiB，按最近使用淘汰，只作用于带 `.audiolib-downloads` 标记的库目录和其中 `<id>-<16 位十六进制>` 条目目录，不跟随链接，不触碰 `cache/` 下其他内容（如 `cache/waveforms`）。
- CLI：`remote edit <id[,id…]> [--tags] [--tag-mode replace|append|remove] [--color '#RRGGBB'|''] [--notes] [--favorite]`（单 id + replace 走 PATCH，否则走批量），`remote plays <id>`，`remote download <id[,id…]>`；输出 `revision_before`/`revision` 和缓存结果（`applied` 或执行的同步）。
- WebDAV：每个扫描根一行，可编辑。有效地址优先级：**用户填写的地址（按根名保存）> 服务端 `/roots.webdav_url`（仅为默认建议）> 全局 `base_url + "/" + 根名`**；留空或与服务端建议相同即表示使用服务端默认。记录路径按最长 `server_path` 前缀（按路径分量匹配）替换为有效地址，剩余部分逐段百分号编码。「测试连接」按根检查有效地址（PROPFIND Depth:0，405/501 时回退 HEAD），并标明来源（用户/服务器/基础地址）。WebDAV 是可选项（试听默认走 §8 服务器流）：未配置地址的根显示「– 未配置 WebDAV，试听走服务器流」，已配置但不可达只算警告（!），都不让测试整体失败。测试另外用本机已同步快照中的一条未缺失记录请求 `GET /audio/{id}/stream`（`Range: bytes=0-0`）：206/200 通过，401、503（含根离线 `ROOT_OFFLINE`）、不可达和超时算失败（超时附「磁盘可能正在唤醒」提示），404（该记录文件缺失）算警告；没有该库快照时跳过。`AudioLibCli remote test` 只在必需步骤失败时返回 `REMOTE_TEST_FAILED`，警告放在 `warnings` 数组。
- 试听（内存流式，不写磁盘）：选中记录后音频只放在进程内存中（全局 LRU，默认 512 MiB，`data_source.audition_memory_mb` 可设 32–8192；按整条淘汰，正在读的条目不淘汰），键 = 库 id + 记录 id + 文件大小 + 路由（如 `pcm-48000-16-2`）+ 来源（server/webdav），复用时缺失部分用 `If-Range`（强 ETag）续取，版本变化则整条重来，不会混用两个版本。旧版本的磁盘试听缓存 `<library_id>/audio-cache/` 在启动/切换服务器时删除一次。
  - 是否转码由客户端决定，按服务器配置单独保存：「数据源与音效库」窗口连接页中的「试听时转码（适合外网或低带宽）」（`data_source.profiles[].transcode`），以客户端设置为准，吞吐测量不会自动打开或关闭它。**不勾选**：试听始终 `GET /audio/{id}/stream`（Range、内存 LRU、缓冲状态照常），从不请求 `/preview`。**勾选**：按下面的预览路由。默认值：http 局域网地址（私有/回环/链路本地 IP、`.local`、单段主机名）和新建的局域网配置不勾选；https 或公网主机、以及「使用服务器外网地址」新建的外网配置勾选。旧设置迁移时按地址给默认值，旧版本的 `data_source.audition_original: true` 迁移为不转码；此后 `audition_original` 仅作为当前配置的反值写回，供旧版本读取。`AudioLibCli remote status` 输出 `active_profile.transcode` 和 `audition_transcode`，`remote profiles` 列出每个配置的 `transcode`。连接测试的「试听预览吞吐」一步不受此设置影响，始终抽样 `/preview`。
  - 预览路由（§8.1，仅勾选转码时）：WAV 用 `GET /audio/{id}/preview?codec=pcm&sr=48000&bits=16`（44.1 kHz 系列用 `sr=44100`，不高于源采样率；超过 2 声道且播放第 1 对声道时加 `ch=2`）；`sr`、`bits` 总是带上（只写 `codec=pcm` 会保持原始 96k/24）；非 WAV 文件仍走 `/stream`。吞吐 < 2.5 Mbit/s 时用 `codec=flac`（同 `sr`、16 bit、立体声；< 0.8 Mbit/s 时 `ch=1`）。`415` → 改用 flac；flac 也失败（`415` 或 `503 TRANSCODER_UNAVAILABLE`）→ 原始 `/stream`；旧服务器无 `/preview`（404/400）→ `/stream`。`503 BUSY` 按 `Retry-After` 重试。吞吐估计 = 各次传输（流、预览、flac、快照）中 ≥ 256 KiB 的分段的指数滑动平均（α 0.35），首次测量前按 4 Mbit/s 保守估计；换服务器时清零。pcm 的 ETag 当作不透明字符串（服务器 0.3.5 为 `"<源>-pcm2-…"`），flac 的弱 ETag（`W/"…-flac1-…"`，随 `t=` 变化）不参与版本判断；flac 长度取首个响应的 `X-Total-Frames`（或 `X-Duration`）加上 `X-Start-Frame`（两者都按输出采样率、从起始位置计），没有这些头的旧服务器用快照中的时长。
  - 预读：文件 ≤ 64 MiB 时整条尽快读完；更大的文件读到播放位置之前约 60 秒（窗口上限为内存预算的 3/4，播放位置之后保留 1/4），超出后暂停读取（TCP 背压，保持 15 秒后关闭连接，接近时再用 `Range` 续取）。块大小 256 KiB；每次读取等待数据超过 30 秒算断线（不再用整个响应 30 分钟的总时限）。
  - 播放：解码器只在下一块数据已在内存时才读（`is_buffered`），引擎工作线程从不等待网络，状态、暂停、seek、音量照常响应。开始和 seek 后先缓冲 0.5 秒（或文件剩余部分）；播放中缓冲耗尽时 12 ms 淡出并暂停输出、状态栏 350 ms 后显示「正在缓冲音频…」，攒够 2 秒（或剩余部分）后淡入继续，没有逐帧补静音造成的碎断。播放用的 WAV 头解析读到 `data` 块为止，不为尾部元数据去取文件末尾（以前每次选择要 3 个连接）；Load 只打开一次源。
  - seek：目标不在内存时立刻新发请求——pcm/原始流用 `Range: bytes=<偏移>-`，flac 用 `t=<秒>`（比目标早一帧，按 `X-Start-Frame` 对齐，前面补静音；`416` 视为文件尾的静音）；距离正在接收的位置不超过约 0.3 秒传输量（64 KiB–1 MiB）时等待现有连接。时长与位置始终按原始记录显示（预览与原始文件秒数相同）。
  - 快速切换：服务器模式下只有停留 180 ms 的选择才发起网络请求和引擎 Load；新的选择会关闭旧请求的连接（服务器随即释放名额），同一时刻最多一条试听流 + 一个波形请求。拖出始终从下载缓存提供原始全质量 WAV（内存中已有完整且 ETag、长度一致的原始 `/stream` 副本时直接复制）。
  - 连接复用：所有请求（API、快照、波形、试听流、拖出下载、WebDAV）共用进程内一个 keep-alive 连接池（HTTP/1.1，每个服务器最多保留 4 条空闲连接，空闲 30 秒关闭，复用前探测对端是否已关闭），正常读完的响应把连接放回池中，下一次选择/波形请求不再重新握手（经外网反代 RTT ≈ 175 ms 时新连接到首字节约 0.5 秒）。取消（切换选择、停止）或 seek 放弃的响应会关闭其连接（服务器随即释放名额）；试听流开始和被取消时后台补一条 `GET /api/v1/health` 预热连接放入池中（同一时刻最多两条），供下一次请求使用。取消只关闭该请求当前使用的连接，不会误关池中的空闲连接。首字节等待仍按 NAS 唤醒放宽（最多 100 秒），正文按每次读取 30 秒无数据判断断线，可容忍外网抖动。
  - 其余规则不变：只有用户为该根配置了 WebDAV 地址时，服务端流失败（含 `503 ROOT_OFFLINE`）才改走 WebDAV（原始文件）；`/roots` 已报该根离线时先试 WebDAV。首个响应最多等 100 秒（NAS 唤醒），1.5 秒后显示「正在唤醒音效库磁盘…」；切换或停止立即取消并关闭连接；`503 ROOT_OFFLINE` 首个响应前立即提示且不循环重试，首个响应后的断线、超时、`503` 重试 3 次（0.5/1/2 秒，不早于 `Retry-After`）。音频回调中没有网络、文件 I/O、锁或分配。
- 波形：服务器模式下 `GET /audio/{id}/waveform?buckets=<像素宽>&colors=1&start_frame=&end_frame=`（`Accept-Encoding: zstd`，`If-None-Match`），与音频解耦单独绘制：内存缓存（按 revision）→ 磁盘缓存 `cache/waveforms/server/<库>/<键>.json` + `.etag`（有则立即发条件请求，`304` 直接用）→ 无缓存时等音频预缓冲完成（最多 3 秒）再请求，以免拖慢首个声音。服务器拿不到波形时，只有在试听数据已完整在内存中时才本地计算，不会为波形整条下载。
- 本地专属能力保持本地：播放引擎、ASIO/WASAPI、SoundTouch 试听、命名管道 IPC（仅本机，不监听网络）。

## 12. 待决问题

1. 文件 ID 用数据库 id 还是相对路径？（草案：数据库 id，路径只做显示）
2. 分页 v1 用 offset 还是游标？（草案：offset，游标预留）
3. ~~服务端端口、部署方式~~：已定，macOS 原生，端口 8790（§10）。
4. `audiolib-audio` 拆分：解码 + 元数据（跨平台）和播放（CPAL + `audiolib-windows`，feature 门控）。远程扫描已不再等待这次拆分：服务端为波形早已依赖 `audiolib-audio`，现在直接启用 `audiolib-db` 的 `scanner` feature（从不打开音频设备）；拆分只剩减小服务端体积的意义。
5. ~~波形 JSON 还是二进制~~：已定 JSON + 可选 zstd（§9，压缩后约为 JSON 的 1/10）。
6. ~~token 轮换和是否需要 HTTPS~~：已定，局域网 HTTP 免 token，外网经 HTTPS 反代 + 管理页生成/撤销的 token（§2.2–§2.4）。以后可加：管理密码、token 过期时间、客户端信任自签名证书。
7. 库身份是否迁入未来 schema（§3）。
8. 增量同步 `GET /library/changes`（§6.5 草案）：变更日志放库内触发器（schema v5）还是服务端旁路；保留多久；410 的阈值。
9. 列表缩略波形（§9.1–§9.3，服务端已实现）：客户端评审结论见 §9.3；按需队列的让路已定（只在试听开始后的前 3 秒让路，试听期间最多 1 个解码名额，不因扫描暂停；预生成在试听整个期间和扫描期间让路，§9.2）。`list_waveform_cache_mb` 默认 640 MiB 已确认够用：909,342 条的满库约 465 MiB（§9.1 容量估算），超出时按分片淘汰并在 `/waveforms/status` 报 `over_budget`。
10. ~~列表波形缓存管理（§9.5）~~：已与客户端评审定：停止用 `POST …/stop`；`rebuild` 需 `confirm`；暂停跨重启保留；失败明细不持久化，重启后 `failed` 回退为 `missing`（`job.mode_effective`）；HTTP ETA = 剩余 / 60 秒窗口速度（当前桌面另用客户端滚动估计）；暂不提供 `GET /waveforms/jobs/{id}`。

- [Schema v6 合并升级契约](Schema_V6_Iteration.md)：修订后的结构升级、本机历史与兼容验收目标。

- [v6 逐列审计](Schema_V6_Column_Audit.md)：57 项列用途与保留建议，不授权额外删除。
