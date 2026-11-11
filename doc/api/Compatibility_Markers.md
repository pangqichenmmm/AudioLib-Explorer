# 基于标记的同步兼容判定（历史设计）

状态：**历史设计记录**（[PR #97](https://github.com/Freedragonlancer/AudioLib-Explorer/pull/97)，不再作为待合并实现）。当前 `audiolib-core/src/compatibility.rs` 已按协议标记、完整库格式和 feature 判定兼容性；具备标记的双方不再以应用版本范围作为同步硬门槛。本文保留设计时的问题、方案与示例，不代表当前完整字段、格式目录或实现状态。现行契约以 [Library_Migration_Compatibility.md](Library_Migration_Compatibility.md) 和当前源码为准。

## 1. 设计时现状（历史代码对照）

| 位置 | 现行字段/逻辑 | 问题 |
|---|---|---|
| `crates/audiolib-core/src/compatibility.rs` `CompatibilityManifest` | `application_version`、`catalog_revision`、`peer_application_ranges`、`legacy_peer_application_ranges`、`direct_use_/auto_migrate_application_ranges`、`api_protocols`（`"api/v1"`）、`snapshot_read_formats`、`database_write_formats`、`migration_*`、`minimum_sqlite_version` | 版本范围是硬门槛 |
| `peer_versions_compatible()` | 双向检查对方版本落在 `peer_application_ranges`，且 `catalog_revision == CATALOG_REVISION`，且 `api_protocols` 有交集 | 任一侧发新 minor（如 0.8.0）即拒绝，即使协议和格式完全一致 |
| `SnapshotFormat::is_readable_by` / `LibraryFormat::snapshot_format_id()` | 格式 ID 由实际库推导（`ale-s6-cld1-t3-m2-p1` 等）并须在 `snapshot_read_formats` 中 | 已是正确的格式判定，保留 |
| `crates/audiolib-remote/src/compatibility.rs` | `peer()`、`library()`、`live_library()`（探测/轮询/每次写入前复查）、`response_format()` 校验快照头 | `peer()` 依赖版本 |
| `server/audiolib-server/src/compatibility.rs` | `admit()`（快照）、`admit_peer()`、`admit_mutation()`、`admit_library_mutation()` | `admit_peer()` 依赖版本，返回 `409 PEER_VERSION_UNSUPPORTED` |
| `server/audiolib-server/src/manage.rs` `CapabilitiesResponse` | `compatibility` + `list_waveforms`、`list_waveform_jobs` 等可选能力 | 可选能力已按“缺失即关闭该功能”处理，保留并统一为 features |

## 2. 三类标记

| 标记 | 类型 | 含义 | 何时变更 |
|---|---|---|---|
| `protocol` | 整数区间 `{min, max}`（当前 `1..=1`，对应 `api/v1`） | API 线协议 | **仅**在不兼容 API 变更时 +1（删除/改名字段或端点、改变语义、改变必需请求头）。新增可选字段/端点不升 |
| 库格式 ID | 字符串（现有 `ale-s{schema}-...`） | 快照/库的持久化格式 | schema（`user_version`）、FTS 存储、必需 feature 行、tokenization/source revision 任一改变 → 新格式 ID。读方只看“能否读该 ID” |
| `features` | `{name: revision}` | 可选能力 | 新增可选功能时添加；语义不兼容地改变某能力时提高其 revision。缺失只关闭该功能，**永不**拒绝整个同步 |
| `application_version` | semver 字符串 | 显示、更新检查 | 不参与兼容判定；仅可命中显式 `blocked_versions` |

## 3. 承载位置

| 标记 | 服务端 → 客户端 | 客户端 → 服务端 |
|---|---|---|
| protocol / features / 可读写格式 | `GET /api/v1/capabilities` 的 `compatibility` 对象；快照响应头 `x-audiolib-server-compatibility`（同一 JSON） | 每次修改请求及快照请求带 `x-audiolib-client-compatibility`（同一 JSON，≤ `MAX_COMPATIBILITY_HEADER_BYTES` 4096） |
| 快照格式 | 快照响应头 `x-audiolib-snapshot-format`（ID）、`x-audiolib-library-format`（`LibraryFormat` JSON）、`x-schema-version`、`x-search-storage-revision`、`x-tokenization-revision`、`x-source-search-revision`、`x-source-tags-revision` | `x-audiolib-accept-snapshot-formats`（逗号分隔 ID）、`x-audiolib-snapshot-schema-max` 等现有上限头 |
| 应用版本 | `/health.version`、`compatibility.application_version` | `compatibility.application_version` |

### 3.1 `compatibility` JSON（新增字段，旧字段在过渡期保留）

```json
{
  "application_version": "0.7.2",
  "marker_revision": 1,
  "protocol": { "min": 1, "max": 1 },
  "api_protocols": ["api/v1"],
  "snapshot_read_formats": ["ale-s3-ext-t3-m2", "ale-s4-ext-t3-m2", "ale-s5-ext-t3-m2",
                            "ale-s6-cld1-t3-m2-p0", "ale-s6-cld1-t3-m2-p1"],
  "database_write_formats": ["ale-s6-cld1-t3-m2-p1"],
  "migration_source_schemas": [3, 4, 5, 6],
  "migration_target_format": "ale-s6-cld1-t3-m2-p1",
  "features": { "waveform_bundle_strong_etag": 1, "list_waveforms": 1, "list_waveform_jobs": 1 },
  "blocked_versions": [],
  "catalog_revision": 1,
  "peer_application_ranges": [">=0.6.0"],
  "minimum_sqlite_version": "3.54.1"
}
```

- 新 manifest **保留** `peer_application_ranges: [">=0.6.0"]`（宽范围，成本低，覆盖残留的版本比较路径），但新代码**从不据此判定**；`direct_use_/auto_migrate_application_ranges` 删除（格式判定已由 `assess_library` 负责）。
- `minimum_sqlite_version`：建议**仅作显示/诊断**（各端各自打开自己的 SQLite，构建已静态链接满足版本，跨端比较无意义）；本地启动时若链接库低于常量应在本地报错，而非作为对端判定。
- `catalog_revision` 仅作信息字段输出，不参与等值判定（见 5.1）。
- 未知字段一律忽略，以便追加 features 不需升 protocol。
- `marker_revision` 存在即“标记对端”；`api_protocols: ["api/vN"]` ⇔ `protocol` 区间，过渡期两者并存。

## 4. 判定算法

```text
fn protocol_ok(local, p):
    return max(local.protocol.min, p.protocol.min) <= min(local.protocol.max, p.protocol.max)

fn peer_ok(local, peer_version, peer_catalog):
    if peer_version in local.blocked_versions: return Err(BlockedVersion)
    match peer_catalog:
        None => return Err(PeerUpgradeRequired)           // 0.4.11/0.5.x 无目录旧端：同样要求升级，不再维护 legacy 路径
        Some(p) if p.marker_revision is none =>
                return Err(PeerUpgradeRequired)           // 0.6.0–0.7.1：对方本就拒绝我们，直接明确提示升级
        Some(p) =>
                if p.application_version != peer_version: return Err(Inconsistent)
                if !protocol_ok(local, p): return Err(ProtocolMismatch)
                return Ok                                  // 不看版本范围、不看 catalog_revision

// 1) 探测 / 轮询（客户端，live_library）
probe():
    caps = GET /api/v1/capabilities  (404 => 无目录旧端)
    peer_ok(local, health.version, caps.compatibility)?
    format_ok(observed_library)?                      // 现有 library()：ID == 推导值 且 ∈ local.snapshot_read_formats
    enabled_features = { f | f ∈ caps.features 且本地支持其 revision }  // 其余功能仅隐藏/降级

// 2) 快照下载
client: 发送 x-audiolib-client-compatibility + x-audiolib-accept-snapshot-formats = local.snapshot_read_formats
server admit(): peer_ok(server, client) && format.id ∈ accepted && format.is_readable_by(client)
                否则 409 SNAPSHOT_FORMAT_UNSUPPORTED / PROTOCOL_UNSUPPORTED
client response_format(): 头部互相一致 且 id ∈ local.snapshot_read_formats；
                          下载后 downloaded() 复核文件实际格式 == 头部，否则保留旧缓存

// 3) 修改请求
client: 每次带 x-audiolib-client-compatibility，事先 live_library() 复查（不缓存准入）
server admit_library_mutation(): peer_ok && 当前库 format.is_readable_by(client)
server admit_mutation()（设置/缓存任务）: 有头则 peer_ok；无头豁免仅限设置/缓存任务，库写入（admit_library_mutation）一律必须带标记头，否则拒绝
依赖某 feature 的请求：服务端未声明该 feature → 客户端不发；服务端收到不支持的 → 404/400，仅该功能失败
```

`direct_use_/auto_migrate_application_ranges` 改由格式判定（`assess_library` 已基于 `LibraryFormat`），版本范围不再参与。

### 4.1 服务端错误码（`server/audiolib-server/src/compatibility.rs`）

均为 HTTP 409，常量定义于 core `compatibility.rs`，由 `PeerRejection::error_code()` 映射（含 `PEER_CATALOG_INVALID`）。新服务端不再返回 `PEER_VERSION_UNSUPPORTED`——旧服务端用该码表示“服务端过旧”：

| 情形 | `error_code` | `sub_code`（提示） | 适用 |
| --- | --- | --- | --- |
| 客户端版本在 `blocked_versions` | `PEER_VERSION_BLOCKED` | — | 快照、设置/缓存任务（带头时）、库写入 |
| 无目录头，或目录无 `marker_revision`（0.4.11/0.5.x、0.6.0–0.7.1） | `PEER_UPGRADE_REQUIRED` | — | 同上；设置/缓存任务无头时豁免；无头时使用固定提示文案 |
| 对端协议区间整体低于本端（对端太旧） | `PEER_UPGRADE_REQUIRED` | — | 同上 |
| 对端协议区间整体高于本端（对端太新，需升级服务端） | `PROTOCOL_UNSUPPORTED` | `SERVER_UPGRADE_REQUIRED` | 同上 |
| 目录头畸形、重复、超过 4096 字节；或自相矛盾：`application_version` 不一致、有 `marker_revision` 却无 `protocol`、`protocol.min > max`、区间相交但 `api_protocols` 不相交 | `PEER_CATALOG_INVALID` | — | 同上 |
| 对端无法读取当前库格式 ID | `SNAPSHOT_FORMAT_UNSUPPORTED` | — | 快照、库写入 |

- 快照上限头名与值来自 core 常量：`SNAPSHOT_SCHEMA_MAX_HEADER`/`SEARCH_STORAGE_MAX_HEADER`/`TOKENIZATION_MAX_HEADER` 与 `SEARCH_STORAGE_MAX`、`TOKENIZATION_MAX`（schema 上限需 ≥ 实际库 schema）。
- 对端标记检查先于 schema < 6 快照的格式豁免执行（矩阵第 3、9 行）。
- 无头豁免仅限设置与缓存任务（及已废弃的 playback 空操作）；库写入缺少目录头一律 `PEER_UPGRADE_REQUIRED`。
- 客户端 `features` 只记入 `tracing::debug!` 诊断日志，不影响准入或响应。

### 4.2 遗留兼容字段（仅为旧客户端显示升级提示而保留，可在日后移除）

旧客户端先解析 `GET /api/v1/health` 与 `GET /api/v1/capabilities`，再发出会得到 409 的请求。若探测响应缺少它们 struct 中的必填字段（无 `serde(default)` 且非 `Option`），整个响应解析失败，用户只看到解析错误而非升级提示。逐 tag 核对 v0.4.11–v0.7.1 的客户端 struct（`HealthInfo`、`Capabilities` 及嵌套的 `ListWaveformsCapability`/`WaveformJobsCapability`、`CompatibilityManifest`、`RemoteLibrary`/`SnapshotFormat`/`LibraryFormat`、`ErrorBody`）：

| 版本 | 必填但新服务端曾缺失的字段 | 备注 |
| --- | --- | --- |
| v0.4.11、v0.5.0、v0.5.1 | 无 | `Capabilities`/`HealthInfo`/`ErrorBody` 全部有默认值；无 `compatibility` 字段 |
| v0.6.0–v0.7.1 | `compatibility.direct_use_application_ranges`、`compatibility.auto_migrate_application_ranges`（`Vec<String>`） | 同一 struct 也用于解析 `x-audiolib-server-compatibility` 响应头 |

其余必填字段（`application_version`、`catalog_revision`、`peer_application_ranges`、`api_protocols`、`snapshot_read_formats`、`database_write_formats`、`migration_source_schemas`、`migration_target_format`、`migration_protocols`、`minimum_sqlite_version`）新服务端仍输出，类型一致，无跨版本类型冲突。

- `direct_use_application_ranges`、`auto_migrate_application_ranges` 为**遗留兼容字段**：值同 `peer_application_ranges`，只为让旧客户端能解析；任何准入判定都不读取它们（测试 `legacy_compat_ranges_never_affect_admission`）。旧客户端也只用自己本地的这两个字段。
- 回归 fixture：`server/audiolib-server/tests/legacy_client_shapes.rs` 逐字复制 v0.4.11（最旧）、v0.5.x、v0.6.0–v0.7.1（最新）形状。
- 当不再需要向 0.6.0–0.7.1 客户端显示升级提示时，可删除这两个字段及对应 fixture。

## 5. 变更规则与发版检查清单

标记：**API protocol 区间**、**格式 ID（含 schema）**、可选 **features**。

### 5.1 `catalog_revision` 与格式 ID 的区别

- **格式 ID**（`ale-s6-cld1-t3-m2-p1`）：描述**一个库文件的实际持久化格式**，由 `LibraryFormat::snapshot_format_id()` 从文件推导；读方据此决定能否直接使用快照。
- **`catalog_revision`**（`CATALOG_REVISION = 1`，现行文档称“目录语义版本”）：描述**兼容目录（manifest）本身的规则语义**，与某个库无关，也不是库的数据版本（库的数据版本是 `x-library-revision`）。
- 目录语义的变化要么改变 API 交互（→ protocol），要么改变可读格式集合（→ 格式 ID 列表），二者已覆盖；manifest 的 JSON 形状由第 7 节 DTO 快照测试守护。现行 `peer_versions_compatible` 中 `peer.catalog_revision == CATALOG_REVISION` 的**等值**检查（core `compatibility.rs` ~:452）一旦 +1 就会锁死所有旧端。处理：**移出判定**（其语义并入 protocol 与格式 ID），仅输出供诊断（`/health.compatibility_catalog_revision` 保留），下一个主版本删除；若评审坚持保留，则改为声明 `catalog_revisions: {min,max}` 区间相交，而不是等值。

### 5.2 主要代码变更点（实现阶段）

1. `API_PROTOCOL: &str = "api/v1"` → `API_PROTOCOL_MIN/MAX: u32`，manifest 输出 `protocol: {min,max}`，过渡期同时输出 `api_protocols: ["api/v1"]`。
2. `peer_versions_compatible()`：删除应用版本范围检查和 `catalog_revision` 等值检查，只保留 `blocked_versions` + protocol 相交 + manifest 自洽；格式层 `is_readable_by` 不变。
3. 服务端快照准入的上限头（`server/audiolib-server/src/compatibility.rs:37-51` 的 `x-audiolib-snapshot-schema-max`、`x-audiolib-search-storage-max`、`x-audiolib-tokenization-max`，目前手写 `1`、`3`）改为从 core 常量派生（`TARGET_SCHEMA`、`LibraryFormat::target().features["search_storage"]`、`TOKENIZATION_REVISION`）；客户端发送端同样由这些常量生成，并对响应头做对称校验。
4. 删除 core 测试 `fixed_0_7_0_and_0_7_1_catalogs_accept_each_other`（~:621）：minor 升级后它比较的是两份相同派生范围，恒真、已无意义（且与 6.1 实测的旧端行为相反）。同理清理其余依赖版本范围的测试。

### 5.3 何时变更

| 标记 | 必须变更 | 不得变更 |
|---|---|---|
| `protocol.max`（+1） | 任一 API DTO（请求/响应 JSON、manifest、错误体）删除/改名/改类型/改语义；端点删除；新增**必需**请求头；旧端合法请求会被拒或误解 | 仅新增可选字段/端点（改为加 feature） |
| `protocol.min` | 不再服务旧协议时 | — |
| 新格式 ID | `TARGET_SCHEMA`/`user_version`；`LibraryFormat::target()` 任一 feature/revision；`SearchStorage`；`TOKENIZATION_REVISION`；source search/tags revision；表/列/索引结构（即使 `user_version` 未变） | 仅应用代码变化 |
| `snapshot_read_formats` | 新 ID 加入；旧 ID 仍可读则保留 | — |
| `features` | 新增可选能力；能力语义不兼容变化 → 该 feature revision +1 | 能力变必需 → 改升 protocol |
| `application_version` | 每次发版 | 永不影响判定（`blocked_versions` 除外） |

### 5.4 发版 PR 检查清单

- [ ] **发布顺序：生产服务端先升级，客户端随后发布。** 服务端须同时服务旧协议区间内的客户端；不得先发需要新 protocol/feature 的客户端。

- [ ] 按 5.2 逐项确认是否需升 protocol / 新格式 ID / 加 feature，并写入 PR 描述。
- [ ] 第 7 节 schema 指纹与 API DTO 快照测试通过；若失败，先改标记，再更新 fixture。
- [ ] 新 feature 在对端缺失时有降级路径和测试。
- [ ] 第 8 节矩阵已回填。

## 6. 从已发布版本过渡

### 6.1 旧端实际检查（源码核对 v0.6.0–v0.6.5、v0.7.0、v0.7.1）

旧端 `peer_versions_compatible` 对对端 manifest 依次要求：① 对端版本落在**旧端自身** `peer_application_ranges`；② manifest `application_version` 与该版本一致；③ `catalog_revision == 1`；④ 对端 ranges 接受旧端版本；⑤ `api_protocols` 含 `api/v1`；快照/写入另需格式 ID 可读。

旧端自身 ranges：

| 已发布版本 | 接受的对端版本 |
|---|---|
| 0.6.0 | `=0.4.11`、`=0.5.0`、`=0.5.1`、`=0.6.0` |
| 0.6.1–0.6.5 | 上述 legacy + `>=0.6.0, <0.7.0` + `=自身` |
| 0.7.0 | legacy + `>=0.6.0, <0.7.0` + `=0.7.0` |
| 0.7.1 | legacy + `>=0.6.0, <0.7.0` + `=0.7.1` |

### 6.2 结论

- ① 只看版本号，新端声明什么都无法通过：**0.7.2（及以后）不能被任何 0.6.x、0.7.0、0.7.1 接受**（除非谎报版本，不采用）。0.7.0 与 0.7.1 彼此也不互认。
- 因此**旧端必须升级**；**从 0.7.2 起，双方只按标记判定**。
- 新 manifest 仍输出 `peer_application_ranges: [">=0.6.0"]`，但无法改变 ①（判定看的是旧端自己的范围）；新端遇到无 `marker_revision` 的 0.6.0–0.7.1 对端直接返回“对端需升级”（`409 PEER_UPGRADE_REQUIRED` / 客户端提示），不再做版本范围比较。
- 无目录旧端（0.4.11/0.5.x）同样要求升级，不再维护单独的 legacy 路径（`legacy_peer_application_ranges` 及相关代码整体删除）。
- **发版顺序**：0.7.2 服务端会拒绝全部 0.6.0–0.7.1 客户端，因此服务端升级与客户端发布必须放在**同一个发版窗口**内完成（先升服务端，随即发布客户端并提示用户更新），不能单独先升服务端。

## 7. 防护测试

- **格式指纹 fixture**：`crates/audiolib-core/tests/fixtures/format_fingerprint.json` 记录 `{format_id: PLAN_B_FORMAT, schema: TARGET_SCHEMA, target: LibraryFormat::target(), ddl_sha256: <新建空库 sqlite_master 中 type/name/sql 排序后的哈希>}`。测试：新建空库算指纹；若 `ddl_sha256` 或 `target()` 变了而 `PLAN_B_FORMAT` 未变 → 失败，提示“改 schema 必须给新格式 ID 并加入 snapshot_read_formats”。
- **API DTO 快照**：对所有公开 DTO（`CapabilitiesResponse`、`CompatibilityManifest`、`/health`、`/library`、快照/修改请求与响应、错误体等）用 `schemars` 生成 JSON Schema，连同快照头名和修改请求必需头，存为 `tests/fixtures/api_v{protocol}.schema.json` 并以 `protocol.max` 为键。测试：当前生成结果与 fixture 比较；若有**删除/改名/改类型/变必需**而 `protocol.max` 未变 → 失败；仅新增可选字段 → 提示更新 fixture（可接受）。
- **版本无关性**：两份标记 manifest 只改 `application_version`（`0.7.2`↔`9.0.0`）及 `catalog_revision`，互验必须通过。
- **旧目录 fixture**：保存真实 0.6.5、0.7.0、0.7.1 的 manifest，新端须返回明确的“对端需升级”，而非笼统失败。

## 8. 测试矩阵（客户端填写）

| # | 客户端 | 服务端 | 期望 | 探测 | 快照 | 修改 | 备注 |
|---|---|---|---|---|---|---|---|
| 1 | 0.7.2 | 0.7.2 | 通过 | | | | |
| 2 | 0.7.2 | 0.7.1 / 0.7.0 / 0.6.x | 拒绝，提示服务端升级 | | | | |
| 3 | 0.7.1 / 0.7.0 / 0.6.x | 0.7.2 | 拒绝（旧端版本检查），新服务端写入/快照返回 PEER_UPGRADE_REQUIRED | | | | |
| 4 | 0.7.2 | 标记端 0.8.0，同协议同格式 | 通过 | | | | |
| 5 | 标记端 | 协议区间不相交 | 拒绝 `PROTOCOL_UNSUPPORTED` | | | | |
| 6 | 标记端 | 同协议，不同 `catalog_revision` | 通过 | | | | |
| 7 | 标记端 | 未知格式 ID | 快照拒绝，保留旧缓存 | | | | |
| 8 | 标记端 | 缺 `waveform_bundle_strong_etag` | 同步通过，仅该功能降级 | | | | |
| 9 | 标记端 | 版本在 `blocked_versions` | 拒绝，明确提示 | | | | |
| 10 | 0.7.2 | 0.5.1（无目录） | 拒绝，提示升级 | | | | |
| 11 | 快照头与文件实际格式不一致 | — | 拒绝，保留旧缓存 | | | | |
| 12 | 标记端（某 feature revision 较低） | 标记端（同一 feature revision 较高） | 仅关闭该功能，同步照常 | | | | |

## 9. 已定默认（评审可修改）

1. **已定默认**：`skipped_newer` **不是** feature，仍留在 `audiolib-update` 更新检查结果中。
2. **已定默认**：旧字符串字段 `api_protocols: ["api/v1"]` 在下一个主版本删除，此后只用整数 `protocol` 区间。
3. **已定默认**：`blocked_versions` 先写死在程序常量中；随更新检查下发留作以后再议。
4. **已定默认**：0.4.11/0.5.x 无目录旧端同样要求升级，不再维护单独 legacy 路径，legacy 代码整体删除（矩阵第 10 行期望已确定）。
5. **已定默认**：客户端也声明自己的 `features`（在 `x-audiolib-client-compatibility` 中），客户端某功能仅在双方都声明时启用；服务端只用客户端 features 做诊断日志，不据此改变响应。
6. **已定默认**：允许 `schemars`，仅作 dev/test 依赖，用于 API DTO JSON Schema 快照。
7. **已定默认**：`minimum_sqlite_version` 仅作显示；另在本地启动时自检链接的 SQLite 版本，不参与对端判定。
8. **已确认**：新 manifest 声明宽范围 `peer_application_ranges: [">=0.6.0"]`，新代码从不据此判定。
