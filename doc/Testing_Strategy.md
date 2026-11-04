# AudioLib Explorer - Testing Strategy

## 当前 Rust 验证策略

按改动风险选择最小充分验证，并记录执行范围和结果。[Rust Validation](Rust_Validation.md) 中的历史结果是已执行证据，不是每次开发都要重跑的清单。保留现有正式测试；GitHub 常规 CI 只检查格式，所有功能测试在本地按范围执行，不增加测试依赖。

| 改动范围 | 日常本地验证 |
| --- | --- |
| 纯文档、注释 | 检查内容、链接和 `git diff --check`；不运行 Cargo、发布构建或安装。 |
| CI 工作流、验证脚本 | 检查 YAML/PowerShell 语法和受影响命令路由/冒烟测试；应用二进制未改时不重建、重装。 |
| 单个 Rust 模块或功能 | 格式检查一次，受影响 package 的严格 Clippy，以及覆盖改变行为的定向测试。 |
| UI 文案、布局、图标及资源 | 编译受影响桌面目标，按实际改动和风险选择相关交互或视觉验收；涉及 Rust 代码时加格式和严格 Clippy。只有真实状态、事件或行为回归才新增测试。 |
| 公共 DTO、DataProvider 或跨层 contract | 验证实现层和直接消费者；按调用关系扩大 package 和测试范围。 |
| 锁文件、共享构建配置或影响范围无法判断 | 本地执行全 workspace 的格式、严格 Clippy 和测试。 |
| 持久化、并发写入、路径或音频后端 | 定向测试加下表对应回归；不能用一次界面启动替代数据完整性验证。 |

先读改动和直接调用方，再选命令。下列示例使用仓库现有 package 和测试名称，按任务选用，不是依次执行的清单：

```powershell
# Rust 改动：格式检查一次；仅对受影响 package 做严格 Clippy。
cargo fmt --all -- --check
cargo clippy -p audiolib-db --all-targets --locked -- -D warnings

# 核心输入 contract：只运行相关单元测试。
cargo test -p audiolib-core --lib --locked batch_rejects_empty_fields_and_invalid_ids_before_writing

# 库管理改动：选择相关集成测试文件；仅一个行为变化时可继续按名称过滤。
cargo test -p audiolib-db --test library_management --locked
cargo test -p audiolib-db --test scan_management --locked unicode_nonrecursive_cleanup_counts_only_newly_missing_files

# UI 文案/布局需要确认 Slint 和资源能够编译；已有等效编译结果时不重复执行。
cargo check -p audiolib-desktop --bin AudioLibExplorer --locked
```

Clippy 已检查的目标不再额外跑相同配置的 `cargo check`；测试、构建和 Clippy 目的不同，但每一步只选一次所需范围。不要为了增加数量而写与实现镜像的断言或重复用例；优先补真实错误、边界、状态转换和用户行为的回归。

### 重型回归的触发条件

| 检查 | 何时运行 |
| --- | --- |
| `tools/validate-review-fixes.ps1` | 修改扫描协调、配置持久化或 Metadata 保存等行为时。普通布局、文案变化不触发。 |
| `tools/validate-library-management.ps1` | 修改备份/恢复、schema、重链接/移除、导入/导出、扫描历史或多库管理时。相关 `library_management`、`schema_v5`、`administration_isolation` 等定向测试仍需按影响运行。 |
| `tools/validate-playback-ipc.ps1` | 修改实际播放、transport 或跨进程控制行为时。 |
| `tools/validate-runtime.ps1` | 修改旧库迁移、扫描或 WAV Metadata 解码的相关路径，且真实 fixture 可用时；缺失时明确记录未验证。 |
| `tools/validate-access-denial.ps1` | 修改权限错误、路径/文件访问或拒绝写入处理时。真实 ACL 操作限于已验证的自有临时子树，并在 `finally` 恢复。 |
| `tools/validate-desktop-close.ps1` | 修改关闭流程、写入排空或扫描取消时。 |
| `tools/validate-spectral-ui.ps1`、`tools/validate-window-repaint.ps1` | 修改对应的 Spectral/Waveform UI 或重绘行为时。 |
| `tools/test-development-pipeline.ps1` | 修改 `check-local.ps1` 或 `build-release.ps1` 时，用 fake Cargo 验证范围路由、feature 和失败传播，不编译应用。 |
| `tools/test-build-artifacts.ps1` | 修改编译缓存维护或租约时，使用自有临时 Cargo 目录验证预算、保留、链接/路径、归属、占用和交付后清理；不删除真实缓存或用户数据。 |
| `tools/test-release-pipeline.ps1` | 修改 Release 执行器或计划时，使用隔离 Git、伪命令和文件验证参数、短输出、失败续跑、环境/源码/产物失效与独占锁；不运行真实应用构建。 |
| `tools/test-local-release-install.ps1` | 修改固定安装器时验证包拒绝和数据保留；可用已验收运行文件在自有临时 checkout 中验证启动、身份及幂等复制，不动真实安装与工作库。 |
| `tools/test-package-boundary.ps1`、`tools/test-server-package.ps1` | 打包规则变化时在本地运行合成回归；实际包的白名单、隐私与身份检查由本地打包/验收负责。 |
| 百万记录基准、真实库搜索、`tools/validate-gui-search-performance.ps1`、播放起音测量 | 修改性能相关路径、处理性能回归，或用户明确要求测量时。日常功能和文档改动不跑。 |

备份恢复、schema 迁移、扫描锁/管理租约、丢失写入、路径验证和源 WAV 保留属于数据安全边界，必须保留与改动直接相关的回归。`scan_management` 覆盖扫描清理、错误明细和 WAV Metadata 变化；`library_management` 覆盖 WAL 备份恢复、回滚、FTS5 修复、原子编辑、重链接和仅数据库移除。桌面草稿排空与旧请求失效也应在相关状态变化时验证。所有测试数据库与音频样本使用自有临时目录，不能修改工作库、旧库或源 WAV。

性能目标仍为 **P95 latency <500ms for 1M+ records**。报告实际测试规模、条件及测量结果；驱动报告的延迟不能替代按钮到实际出声的测量。不为普通开发验收反复读取或哈希整套真实库和源音频。

### 本地验证与云端最小检查

完成实际编译/测试后使用 `check-local.ps1 -CleanBuildArtifacts` 或独立缓存维护命令，按默认 40 GiB 预算清理 debug；发布在交付完成后用管线同名开关维护。未超过预算不删除，纯文档或复用未变检查不重复扫描。构建/测试期间不清理，失败时保留缓存和诊断；详见[编译缓存维护](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Build_Cache.md)。

GitHub 普通 CI 仅保留一个 Linux 作业，只运行 `cargo fmt --all -- --check`。相关 `main`、`beta`、`codex/**` 推送会触发，纯文档和截图推送跳过；所有 Pull Request 均须返回 required `Linux (fmt)` 结果。`.github/workflows/release.yml` 仅在手动触发时只读核对已发布 Release 的身份、精确资产集合与 SHA256，不构建、不打包、不上传，也不承担运行验收。

开发与发布都按实际改动、直接消费者和发布配置选择验证范围，不因打标签自动扩大到全 workspace。`check-local.ps1 -Package <packages>` 将格式、所选包的严格 Clippy 和测试执行一次；仅影响广泛或无法可靠判断时省略 `-Package`。已有更窄的行为测试时继续直接运行 Cargo 定向测试，不必为使用统一入口扩大范围。

```powershell
# 受影响的 Windows 客户端包；不要再附加无 ASIO 整套检查。
./tools/check-local.ps1 -Package audiolib-desktop,audiolib-cli -Asio
# 受影响的 Linux 服务端；按无播放设备的发布配置测试，不启动客户端矩阵。
./tools/check-local.ps1 -Package audiolib-server -NoDefaultFeatures -Target x86_64-unknown-linux-gnu
```

无 ASIO 条件分支仅在本次改动影响它时，按受影响 package/测试补测；明确交付无 ASIO 版本时才选择不带 `-Asio` 的完整入口。ASIO 实际设备验收只在驱动/后端行为变化时按相关硬件执行。普通业务改动不重复两种构建配置的同一套测试。

| 阶段 | 负责的检查与执行次数 |
| --- | --- |
| Rust 开发/发布检查 | 对所选范围和实际交付配置执行一次；全 workspace 仅用于广泛或不确定的影响。 |
| 专项回归 | 按上表风险追加，保留相关数据安全覆盖；不同专项验证不同边界。 |
| 构建与打包 | 每份 payload 构建、打包一次；打包器检查实际文件边界、隐私、许可证及归档结构。 |
| 包运行验收 | 每份实际 payload 验收一次：Windows 用 `validate-portable.ps1`；Mac 打包加 `-ValidateRuntime` 合并结构和原生验收，成功后不再单独运行验收器。 |
| 本地安装 | 对已验收 payload 核对安装文件哈希、CLI 版本并隔离启动一次；身份与启动通过后不重跑完整 portable。 |
| 发布后远端核验 | 可选手动 workflow，仅核验 Release 身份、资产集合和摘要；不重复本地功能测试。 |

Windows 客户端使用 `tools/build-release.ps1` 构建 desktop/CLI 两个包的 binaries，再运行 `tools/package-portable.ps1 -SkipBuild`。兼容手动入口 `tools/ci-release.ps1 -Tag <tag>` 将这两步和标签身份校验合在一起，并生成运行时为 `not-run` 的报告；二者选一，不串行再跑。它不代替完整 portable 验收。服务端使用专用 `tools/build-server-release.ps1`、打包器及 `tools/validate-server-package.ps1`；Windows 的服务包验收需要构造自有 fixture 时，显式执行 `cargo build -p audiolib-db --bin search-benchmark --release --locked`，不让普通发布构建顺带生成 benchmark。Mac 客户端构建证明保留原构建机路径前缀，便于在另一台 Mac 上核验隐私边界。

原云端 macOS 数据库/服务端生命周期测试、Linux 服务端 Clippy/测试改由匹配平台的本地环境按影响运行。例如在 macOS ARM64 使用 `cargo test -p audiolib-db --lib --tests --locked --target aarch64-apple-darwin`；在 Linux 服务端和数据库均受影响时使用 `cargo test -p audiolib-server -p audiolib-db --all-targets --locked --no-default-features --target x86_64-unknown-linux-gnu`。使用原生 Rust 工具链覆盖仓库默认 Windows target；缺少相应主机或音频硬件时，明确记录未验证，不能以云端冒烟通过代替。

应用变更交付仍需从最新源代码构建、打包并更新固定的 `local_release/`；构建和打包保持同一 ASIO 选择。完整 portable 验证在包目录执行一次，安装后核对所有包内文件（含两份 EXE）的 SHA256、CLI 版本及一次隔离启动，避免对同一有效载荷重复整套检查。隔离启动须使用临时数据目录及显式设置，防止发现工作库或旧库；结束后恢复正常环境。保留正常数据目录、设置和工作库，不能以安装为由覆盖运行时数据。具体安装流程以仓库 [AGENTS.md](../AGENTS.md) 为准。

无 ASIO 的本地交付须在 build、package 和 `validate-portable.ps1` 三步都传 `-WithoutAsio`。默认验收仍要求 ASIO 许可证，不根据文件缺失推断构建特性。打包与 portable 验收共用 `validate-portable-licenses.ps1`，在替换有效包或启动应用前拒绝缺失许可证；其回归包含在 `test-package-boundary.ps1` 中。

`validate-portable.ps1` 默认使用隔离合成数据，`-CleanCheckout` 保留为兼容写法；仅旧库导入路径受影响时使用 `-LegacyImport`。不再因为本机恰有旧库，就在每次 UI 或发布流程验证中重复导入及哈希它。

包级测试已包含的定向用例不在其前后再跑一遍；同一产物在打包目录、安装目录和下载目录之间只需验证新边界的身份，复用仍有效的运行结果。纯重打包若所有可执行文件、动态库、运行资源和启动配置的哈希都未变，可以复用运行验收并重新核对包边界/隐私/归档身份；只比 EXE 哈希不足以证明界面资源或启动配置未变。合成打包规则回归仅在对应脚本或规则变化时运行。跨平台测试保留平台特有路径，Linux 原生包与 Docker 容器分别检查本机依赖和容器运行边界；不把一端通过当作另一端通过，也不为它们重复整套 Cargo 业务测试。

相关应用/测试源码、feature、工具链和环境未变时可复用已通过的检查；标签或文档变化本身不触发 Cargo 重跑。修复后只重跑失败和受影响的检查，不重新启动其他已通过套件。正式产物仍须从最终 main/tag 提交构建并针对实际包验收，不能用旧 beta 二进制代替最终身份；这不要求重跑仍有效的源码功能矩阵。仅在修改相关路径或结果失效时扩大范围。交付记录说明已运行项目、未覆盖范围及原因，不能把未运行的检查写成通过。

Beta 的自动 refresh/smoke 流程见 [AGENTS.md](../AGENTS.md#beta-testing-flow)。GUI 验收围绕当版实际 UI 改动和风险选择，不为每个候选默认要求历史固定清单的人工确认；仅对确需人工观察的具体风险请求确认。纯文档或流程规则更新复用相关、未失效的应用验证证据，不重建或重装未变二进制，也不重复无关 GUI 验收；复用时保留原源码提交和二进制身份。必要自动化检查或所选验收的真实失败仍须处理，不能据此跳过数据安全或版本身份验证。

## 历史设计参考

以下 Version 1.0 正文保留旧 Python 设计与示例；覆盖率比例、测试清单和 Python 命令不作为当前 Rust 每次开发的执行要求，也不恢复旧运行时。

**Version:** 1.0
**Date:** 2025-11-24
**Status:** Testing Specification

---

## Test Pyramid

```
      /\
     /  \  E2E Tests (5%)
    /────\
   /      \ Integration Tests (15%)
  /────────\
 /          \ Unit Tests (80%)
/────────────\
```

**Target Coverage:** 70%+ unit test coverage

---

## 1. Unit Tests

### 1.1 Test Framework
- **Framework:** pytest 7.4+
- **Coverage:** pytest-cov
- **Mocking:** pytest-mock, unittest.mock

### 1.2 Test Structure

```
tests/
├── unit/
│   ├── test_scanner.py
│   ├── test_search_engine.py
│   ├── test_metadata_extractor.py
│   ├── test_waveform_renderer.py
│   ├── test_validators.py
│   ├── test_query_sanitizer.py
│   └── test_i18n.py
├── integration/
│   ├── test_scan_workflow.py
│   ├── test_search_workflow.py
│   └── test_database.py
├── e2e/
│   └── test_main_workflow.py
├── performance/
│   └── test_search_performance.py
└── conftest.py  # Shared fixtures
```

### 1.3 Example Unit Tests

```python
# tests/unit/test_search_engine.py
import pytest
from src.search.search_engine import SearchEngine
from src.database.models import AudioFile

@pytest.fixture
def test_db_session():
    """Create test database session"""
    # Implementation
    pass

def test_search_basic_query(test_db_session):
    """Test basic keyword search"""
    engine = SearchEngine(test_db_session)
    results = engine.search("explosion")

    assert len(results) > 0
    assert all('explosion' in r.filename.lower() for r in results)

def test_search_with_filters(test_db_session):
    """Test search with sample rate filter"""
    engine = SearchEngine(test_db_session)
    results = engine.search("explosion", filters={'sample_rate': 48000})

    assert all(r.sample_rate == 48000 for r in results)

def test_search_performance(test_db_session):
    """Test search meets performance target"""
    import time
    engine = SearchEngine(test_db_session)

    start = time.time()
    results = engine.search("footstep")
    latency = (time.time() - start) * 1000

    assert latency < 500, f"Search latency {latency}ms exceeds 500ms target"
```

---

## 2. Security Tests

### 2.1 Input Validation Tests

```python
# tests/unit/test_validators.py
import pytest
from src.utils.validators import InputValidator, ValidationError

def test_sql_injection_prevention():
    """Test SQL injection attempts are blocked"""
    malicious_queries = [
        "'; DROP TABLE audio_files; --",
        "1' OR '1'='1",
        "'; DELETE FROM audio_files WHERE '1'='1",
    ]

    from src.search.query_sanitizer import QuerySanitizer

    for query in malicious_queries:
        with pytest.raises(ValidationError):
            QuerySanitizer.sanitize_fts5_query(query)

def test_path_traversal_prevention():
    """Test path traversal attempts are blocked"""
    malicious_paths = [
        "../../etc/passwd",
        "..\\..\\windows\\system32\\config\\sam",
        "~/../../etc/shadow",
    ]

    for path in malicious_paths:
        with pytest.raises(ValidationError):
            InputValidator.validate_file_path(path, allowed_roots=["/library"])
```

---

## 3. Performance Tests

### 3.1 Search Performance Benchmark

```python
# tests/performance/test_search_performance.py
import pytest
import time
import numpy as np
from src.search.search_engine import SearchEngine

@pytest.mark.performance
def test_search_latency_100k_records(benchmark_db_100k):
    """Benchmark search on 100K records"""
    engine = SearchEngine(benchmark_db_100k)

    queries = ['explosion', 'footstep', 'door', 'ambient']
    latencies = []

    for _ in range(100):
        query = np.random.choice(queries)
        start = time.time()
        results = engine.search(query)
        latency = (time.time() - start) * 1000
        latencies.append(latency)

    p50 = np.percentile(latencies, 50)
    p95 = np.percentile(latencies, 95)
    p99 = np.percentile(latencies, 99)

    print(f"\nSearch Performance (100K records):")
    print(f"  P50: {p50:.1f}ms")
    print(f"  P95: {p95:.1f}ms")
    print(f"  P99: {p99:.1f}ms")

    assert p95 < 500, f"P95 latency {p95:.1f}ms exceeds 500ms target"
```

---

## 4. Integration Tests

```python
# tests/integration/test_scan_workflow.py
def test_complete_scan_workflow(temp_audio_files):
    """Test end-to-end scan workflow"""
    scanner = ScannerService(session)

    # Scan directory
    stats = scanner.scan_directory(temp_audio_files)

    # Verify results
    assert stats['files_added'] == 10
    assert stats['files_failed'] == 0

    # Verify database
    count = session.query(AudioFile).count()
    assert count == 10

    # Verify FTS index
    results = session.execute(
        "SELECT * FROM audio_search WHERE audio_search MATCH 'test'"
    ).fetchall()
    assert len(results) > 0
```

---

## 5. Test Execution

### 5.1 Run All Tests
```bash
pytest tests/
```

### 5.2 Run with Coverage
```bash
pytest --cov=src --cov-report=html tests/
```

### 5.3 Run Performance Tests Only
```bash
pytest -m performance tests/performance/
```

---

**END OF TESTING STRATEGY**

