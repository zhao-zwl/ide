# Agentic IDE — M1 最小可跑切片 (v0.1 内核验证)

> 范围：T01 宿主解耦 Core · T02 ProtoBus · T03 存储底座 DDL · T04 NES 探针。
> 依据：`docs/development_plan.md`（阶段 0 / v0.1 内核验证）。架构师详细设计未回传，按 SOP 代码实现工作流执行。

本切片验证主轴：**宿主解耦 + ProtoBus + Core(ReAct) + NES 探针** 可端到端跑通，且**不依赖具体 IDE 宿主**。

---

## 1. 仓库布局

```
ide-m1/
├── Cargo.toml                 # workspace：crates/core + crates/probe + crates/cli
├── proto/
│   └── ide_core.proto         # T02 ProtoBus 契约（AgentService + HostService + HealthService）
├── crates/
│   ├── core/                  # T01/T02/T08/T10：宿主解耦 Core + tonic gRPC server
│   │   ├── build.rs          # 编译 .proto -> Rust (tonic-build)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── main.rs        # CLI 入口（ide-core）：demo / serve
│   │       ├── server.rs      # tonic 服务引导（+ HealthService 注册）
│   │       ├── agent.rs       # AgentService 实现 + default_nes_backend（T08/T09）
│   │       ├── config.rs     # T10 Core 运行时配置（env/默认值/单租户开关）
│   │       ├── health.rs     # T10 gRPC HealthService 就绪探针
│   │       ├── host/          # T01 宿主抽象层（provider/bridge/cli_host/grpc_host_client）
│   │       ├── planner.rs / llm.rs / tool_executor.rs / context_manager.rs
│   │       ├── validator.rs / permissions.rs
│   │       ├── chat.rs       # T06 Chat（复用 Llm）
│   │       ├── craft.rs       # T07 人主导编辑（六权校验 + 状态机）
│   │       └── retrieval.rs   # T05 向量检索（纯逻辑）
│   ├── probe/                # T04/T08 NES 探针
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ollama.rs      # OllamaClient（真·/api/generate+/api/embeddings）
│   │       │                 # + NesClient（缓存/降级/批量/超时退避）+ Mock/RuleBased
│   │       └── completion.rs   # LSP 风格补全钩子 + rank_completions + speed_test
│   └── cli/                  # T09 CLI `aidea`（clap 子命令，直链 ide-core）
│       ├── Cargo.toml
│       └── src/{lib.rs, main.rs}
├── deploy/                   # T10 私有化部署底座
│   ├── docker-compose.yml    # core + postgres+pgvector + ollama（健康/卷/环境变量）
│   └── Dockerfile            # 多阶段构建 ide-core + aidea 运行镜像
├── migrations/
│   ├── 0001_init.sql         # T03 PostgreSQL + pgvector DDL
│   └── 0002_v05.sql          # T05/T06/T07 增量迁移
└── README.md                  # 本文件
```

---

## 2. 构建与运行

> M1 不要求真实编译（环境可能无 cargo / PostgreSQL）；代码结构与依赖声明已按可编译方式组织，单测逻辑可概念性 `cargo test` 运行。

### 2.1 构建 / 测试（需 Rust 工具链）
```bash
# 在 ide-m1/ 下
cargo build                 # 编译 workspace
cargo test                  # 运行全部单元测试（权限/排序/向量化/测速桩）
cargo run -p ide-core -- demo "add retry logic to utils"   # 在进程内跑 ReAct 闭环
cargo run -p ide-core -- serve "[::1]:50051"               # 启动 ProtoBus gRPC 服务
```

### 2.2 数据库（需 PostgreSQL 16 + pgvector 0.7）
```bash
createdb agentic_ide
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0001_init.sql
```
脚本幂等（`IF NOT EXISTS` / `ON CONFLICT`），含六权位掩码、审计只追加分区、向量检索函数 `search_context`。

---

## 3. 各模块职责与 T 映射

| 任务 | 模块 | 关键设计 |
|------|------|----------|
| **T01 宿主解耦** | `core/src/host/*` | `HostProvider` trait 定义 Core 所需宿主能力；`CliHost`（桩）与 `GrpcHostClient`（ProtoBus）两种实现证明“换宿主=换实现，Core 零改动”。`HostBridge` 是 Core 唯一入口门面。 |
| **T01 ReAct 骨架** | `core/src/planner.rs` | 决策内核闭环：`Llm.think → Llm.plan_action → ToolExecutor.run → ContextManager.update → Validator`。五组件全部以 trait 注入，可单测。 |
| **T02 ProtoBus** | `proto/ide_core.proto` + `core/src/{agent,server}.rs` | `AgentService`（Core 侧，宿主调用）：`Ping` / `NesComplete`(server-stream) / `RunAgent`(server-stream) / `Chat`。`HostService`（宿主侧，Core 调用驱动 UI）。契约在 v0.1 冻结（G6）。 |
| **T03 存储底座** | `migrations/0001_init.sql` | 六权位掩码 `perm_mask`（domain + SQL 辅助函数，与 `permissions.rs` 同编码）；审计表按月 `RANGE` 分区 + 触发器 + RLS 只追加；`embeddings`(vector(1536)) + ivfflat 索引 + `search_context` 检索函数。 |
| **T04 NES 探针** | `crates/probe/*` | `CompletionBackend`：`OllamaClient`(真·`/api/generate`) + `MockOllamaClient`(确定性占位)。`CompletionProvider` 为 LSP 风格补全钩子；`rank_completions` 纯排序逻辑（已单测）；`speed_test` 测速桩断言 L0 Tab **p95 < 300ms**。 |

---

## 4. 关键设计决策

1. **宿主解耦优先**：Core 仅依赖 `HostProvider` trait 与注入式能力 trait（Llm/ToolExecutor/Validator），不 import 任何具体 IDE。CLI 宿主桩即“无 UI 也能跑通 Core”的证明。
2. **六权位掩码双轨一致**：Rust `PermissionSet` 与 SQL `perm_mask`(0..63) 采用同一 bit 布局（Read=1, Generate=2, Modify=4, Execute=8, Commit=16, Audit=32），DB 约束与代码权威对齐。
3. **审计只追加（等保 G9 左移）**：分区表 + `BEFORE UPDATE/DELETE` 触发器（显式挂到各分区，绕开 PostgreSQL 分区不继承触发器的限制）+ RLS 仅允许 INSERT，三重防篡改。
4. **向量化占位可自包含**：M1 的 `embed()` 用特征哈希生成确定性、L2 归一化向量，维度 1536 与 DDL `vector(1536)` 对齐；后续替换为真实 embedding 模型时调用点不变。
5. **探针可 mock 验证**：`MockOllamaClient` 让 `speed_test` 与排序单测在无 Ollama 环境下即可运行，验证 L0 延迟预算与排序正确性。
6. **gRPC 双向流冻结契约**：补全灰字流、Agent 事件流均以 `stream` RPC 定义，宿主（Tauri/CLI）与 Core 按契约并行。

---

## 5. M1 验收点（退出标准对照）

- [x] **Core 可脱离 UI 独立运行**：`cargo run -p ide-core -- demo` 在进程内以 `CliHost` 驱动完整 ReAct 闭环，零 UI 依赖。
- [x] **HostProvider 可用 mock 驱动**：`CliHost` + `MockLlm` + `BasicToolExecutor` 构成默认栈（`Planner::with_defaults`）。
- [x] **`.proto` 契约冻结且结构正确**：`ide_core.proto` 定义 `AgentService`/`HostService`，含补全与 Agent 双向流；`build.rs` 用 tonic-build 编译。
- [x] **存储 DDL 可执行且覆盖三大要素**：六权位掩码、审计只追加分区、上下文向量检索（`search_context`）均在 `0001_init.sql`。
- [x] **可演示最小闭环**：`demo` 输出 “输入→思考→动作→观察→终态”；`serve` 起 gRPC，`NesComplete` 经 `MockOllamaClient` 返回排序后的灰字。
- [x] **L0 Tab 延迟预算**：`probe::speed_test` 断言 p95 < 300ms（mock 后端确定性通过）。
- [x] **单测基线（T21 贯穿，M1 先建）**：`permissions` / `completion::rank_completions` / `context_manager::embed` / `probe` 测速 均带 `#[test]`/`#[tokio::test]`。

> 后续（v0.5）：接 Tauri 真实宿主、`OllamaClient` 实连、PG/Redis/NATS 一键起、CLI `aidea` 与 IDE 共享配置（T09）。

---

## 6. v0.5 阶段 A（T05 / T06 / T07）增补

> 范围：在 M1 内核上**增量扩展**，不另起炉灶。新增 `chat` / `craft` / `retrieval`
> 三个 Core 模块 + `context_manager` 增强 + `0002_v05.sql` 迁移。宿主仍用
> `CliHost` 桩验证（不引入 Tauri/React 重依赖，UI 留待后续阶段）。

### 6.1 文件清单（相对 M1 的新增 / 修改）

| 文件 | 状态 | 职责 |
|------|------|------|
| `crates/core/src/retrieval.rs` | **新增** | T05 检索/排序纯逻辑：`Retriever` trait、`InMemoryRetriever`、`rank_chunks`/`rank_raw`（复用 `embed`+余弦，镜像 `search_context` 语义）。 |
| `crates/core/src/context_manager.rs` | **修改** | T05 多源上下文：新增 `ContextSource`/`Priority`/`ContextChunk`、`estimate_tokens`、token 预算分级丢弃 `budget_trim`、分层压缩 `compress`、`build_prompt`、`retrieve`；保留原 `embed`/`snapshot`/滚动窗口。 |
| `crates/core/src/chat.rs` | **新增** | T06 Chat：`ChatEngine`（复用 `Llm`）、`ChatSession` 多轮、`Attachment`（`@file`/`@symbol` 解析）、`ToolSuggestion`、流式骨架 `reply_stream`、权限位 `Generate` 校验。 |
| `crates/core/src/craft.rs` | **新增** | T07 人主导编辑：`CraftProposal` 状态机（Suggestion→PendingConfirm→Applied/Rejected）、`CraftEngine`（权限掩码 Modify/Execute/Commit 校验 + 经 `HostProvider::apply_edit` 应用）、`CraftSession` 多文件。 |
| `crates/core/src/agent.rs` | **修改** | T06 实现 `AgentService.Chat`：接入 `ChatEngine`、按 `session_id` 维护 `ChatSession`、流式回答 + 工具建议。 |
| `crates/core/src/planner.rs` | **修改** | T05 接线：新增 `run_with_context`（多源 chunk 注入上下文）、`llm()` 访问器供 Chat 复用同一推理核。 |
| `crates/core/src/lib.rs` | **修改** | 暴露新模块 `chat`/`craft`/`retrieval`。 |
| `migrations/0002_v05.sql` | **新增** | 增量迁移（不改 0001）：`craft_proposals` 表、`context_sources` 表（含优先级/向量索引）、`log_craft_apply` 审计辅助函数。 |

### 6.2 各模块职责与 M1 衔接

- **T05 上下文工程**：`context_manager` 在 M1 滚动窗口之上叠加多源 `ContextChunk`（打开文件/选区/诊断/符号/近期编辑 diff），按 `Priority`（Low→Medium→High）分级丢弃以贴合 token 预算，提供 `compress` 去重/截断；`retrieval` 用确定性 `embed` + 余弦排序，与 PG `search_context` 语义对齐（无 PG 也能跑 demo）。`Planner::run_with_context` 将其接入 ReAct 闭环。
- **T06 Chat**：复用 `Llm` 同一推理核（经 `Planner::llm()` 共享实例），`request→thought→action→tool suggestion` 链路与 ReAct 一致；`@file`/`@symbol` 解析、多轮历史、`Generate` 权限位校验、流式/非流式骨架。proto 契约（M1 冻结 G6）零改动。
- **T07 Craft 人主导**：Agent 仅 `propose` 建议，文件**非用户确认不改**；`confirm` 先经六权位掩码校验（FileEdit→Modify / RunCommand→Execute / Commit→Commit，复用 `permissions.rs`），再经既有 `HostProvider::apply_edit`（`CliHost` 桩已实现）落地。状态机保证 Suggestion→Applied/Rejected 单向转移。

### 6.3 构建与运行（同 M1 工具链，无新依赖）

```bash
# 在 ide-m1/ 下（需 Rust 工具链；沙箱重型编译可能 OOM，但结构/单测逻辑正确）
cargo build                 # 编译 workspace（core + probe）
cargo test                  # 运行全部单测（含 T05/T06/T07 新增）
cargo run -p ide-core -- demo "add retry logic to utils"   # M1 ReAct 闭环
cargo run -p ide-core -- serve "[::1]:50051"               # ProtoBus gRPC（Chat 可经此调用）
```

数据库（需 PostgreSQL 16 + pgvector 0.7）：
```bash
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0001_init.sql
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0002_v05.sql
```

### 6.4 v0.5 阶段 A 验收点勾选

- [x] **T05 多源上下文收集**：`ContextChunk` 覆盖 OpenFile/Selection/Diagnostic/Symbol/RecentEdit 五类源。
- [x] **T05 分层压缩 + 向量检索接入**：`compress` 去重/截断；`retrieve`/`rank_chunks` 复用 `embed`+余弦，语义对齐 `search_context`；`context_sources` 表+ivfflat 索引。
- [x] **T05 token 预算分级丢弃**：`budget_trim` 先丢 Low 再 Medium，High 保护；单测覆盖。
- [x] **T05 与 Planner 接线**：`run_with_context` 注入多源 chunk，单测验证 chunk 进入 prompt。
- [x] **T06 Chat 实现**：`AgentService.Chat` 基于现有 `ChatRequest`/`ChatMessage` 实现；多轮、`@file`/`@symbol` 解析、工具调用建议、流式/非流式骨架；mock LLM 下验证 请求→响应→工具建议 链路（单测）。
- [x] **T06 复用 Planner/LLM**：`ChatEngine` 复用同一 `Llm` 实例。
- [x] **T07 ApplyEdit 流程**：复用 proto `ApplyEditRequest` 形状 + `HostProvider::apply_edit`（`CliHost` 桩）。
- [x] **T07 权限校验**：六权位 Modify/Execute/Commit 校验，缺失即拒绝且不改文件（单测）。
- [x] **T07 Craft 状态机**：Suggestion→PendingConfirm→Applied/Rejected + 多文件 `CraftSession`；单测覆盖确认/拒绝/权限分支。
- [x] **纯逻辑单测可跑**：context（裁剪/排序）、chat（接线）、craft（状态机/权限）均 `#[test]`/`#[tokio::test]`，无外部依赖。
- [x] **M1 契约/默认栈零破坏**：proto 未改；`run`/`demo`/`serve` 行为不变；`CliHost` 行为不变。

---

## 7. v0.5 阶段 B（T08 / T09 / T10）增补

> 范围：在 M1 + v0.5 阶段 A 内核上**增量扩展**，不另起炉灶。本次交付
> **T08 NES 生产化**、**T09 CLI `aidea`**、**T10 私有化部署底座**三块，
> 复用既有 `HostProvider` / `Planner` / `Llm` / `ChatEngine` / `CraftEngine` /
> `Permission` / `Retriever` 抽象，风格与 M1 一致（Google 风格 Rust、async、
> 清晰注释）。宿主仍用 `CliHost` 桩（不引入 Tauri/React 重依赖）。

### 7.1 文件清单（相对 M1/阶段A 的新增 / 修改）

| 文件 | 状态 | 职责 |
|------|------|------|
| `crates/probe/src/ollama.rs` | **重写** | T08 生产化：真·`OllamaClient`（`/api/generate`+`/api/embeddings`、超时+指数退避重试）、`NesClient`（缓存+规则降级+批量并发）、`CachedCompletionBackend`/`DegradingBackend`/`RuleBasedBackend`、`CompletionCache`、`run_batch`、纯请求/解析/退避辅助函数。 |
| `crates/probe/src/completion.rs` | **修改** | T08：抽出纯函数 `derive_rule_candidates`（Mock 与 RuleBased 共享的降级规则）、`rule_complete`，保留 `rank_completions`/`speed_test`/`ProbeCompletionProvider`。 |
| `crates/probe/src/lib.rs` | **修改** | 重新导出 T08 新增公共 API。 |
| `crates/core/src/config.rs` | **新增** | T10：`CoreConfig`（DB URL、模型端点、tenant/单租户开关、日志级别、gRPC 地址），`from_map`/`from_env` 加载+默认值+`validate_tenancy` 单租户校验。 |
| `crates/core/src/health.rs` | **新增** | T10：`HealthService` gRPC 就绪探针实现（`Check` RPC）。 |
| `crates/core/src/agent.rs` | **修改** | T08/T09：新增 `from_config`（按 `CoreConfig` 选 NES 后端）+ `default_nes_backend`（CLI 与 server 共用）。 |
| `crates/core/src/server.rs` | **修改** | T10：注册 `HealthService`；新增 `serve_configured`（按配置起服务）。 |
| `crates/core/src/lib.rs` | **修改** | 暴露 `config`/`health` 模块，re-export `ide-probe` 探针类型（CLI 无需直连 probe）。 |
| `proto/ide_core.proto` | **修改** | T10：**仅追加**最小化 `HealthService` + `HealthCheck*` 消息（G6 冻结契约不动 AgentService/HostService）。 |
| `crates/cli/Cargo.toml` + `src/{lib,main}.rs` | **新增** | T09：clap 子命令 CLI `aidea`（`chat`/`craft`/`nes`/`serve`/`version`），直链 `ide-core` 库。 |
| `Cargo.toml` | **修改** | workspace `members` 注册 `crates/cli`。 |
| `deploy/docker-compose.yml` + `deploy/Dockerfile` | **新增** | T10：core + postgres+pgvector + ollama 三服务（健康检查/卷持久化/环境变量）。 |

### 7.2 模块职责与 M1/阶段A 衔接

- **T08 NES 生产化**：`NesClient` 把真·`OllamaClient` 包成生产后端——请求构造/解析纯函数单测覆盖；`/api/generate` 带 per-request 超时 + 指数退避重试（退避逻辑 `compute_backoff_ms` 纯单测）；`CompletionCache`+`CachedCompletionBackend` 命中缓存降 p95；`DegradingBackend`+`RuleBasedBackend` 在真后端失败时降级到规则补全（始终返回候选，保住 L0 延迟预算）；`run_batch` 用信号量做有界并发批量推理且保序。M1 的 `speed_test`（L0 p95<300ms）保持不变并复用。
- **T09 CLI `aidea`**：与 IDE **共享同一 `ide-core` 决策内核**。`chat`/`craft`/`nes` 直链库（无需常驻 gRPC 服务即可跑通离线）；`serve` 起 ProtoBus gRPC（`AgentServer::from_config` + `HealthService`）；`version` 打印版本与配置。参数解析（`parse_args`）与子命令路由（`dispatch_with`）均可单测。
- **T10 私有化部署底座**：`config.rs` 从 `AIDEA_*` 环境变量/默认值加载并校验单租户；gRPC 侧新增最小化 `HealthService.Check` 就绪探针（compose 用 `grpc_health_probe` 探活）；`docker-compose.yml` 一键起 core+PG+ollama，数据不出域。按 MVP 约定**单租户验证**：仅一个逻辑租户、不做多租户隔离（不重写 0001/0002），`validate_tenancy` 校验 `tenant_id` 非空。

### 7.3 构建与运行（同 M1 工具链，无新重型依赖）

```bash
# 在 ide-m1/ 下（需 Rust 工具链；沙箱重型编译可能 OOM，但结构/单测逻辑正确）
cargo build                 # 编译整个 workspace（core + probe + cli）
cargo test                  # 运行全部单测（含 T08 缓存/降级/批量/退避、T09 路由、T10 配置）

# CLI 直接驱动 Core（无需先起服务）
aidea version                                   # 版本 + 配置概要
aidea chat "where is main?" --attachments @file:src/main.rs
aidea craft --file mem://a.rs --old "let x = 1;" --new "let x = 2;" --kind file --yes
aidea nes --samples 50                          # NES 探针测速，断言 L0 p95 < 300ms
aidea nes --samples 50 --backend ollama         # 真·Ollama 后端测速（需本地 ollama）

# 起 gRPC 服务（含 HealthService）
aidea serve "[::1]:50051"
# 或（core 二进制）
cargo run -p ide-core -- serve "[::1]:50051"
```

### 7.4 私有化一键部署（Docker Compose）

```bash
# 1) 起三服务（core + postgres+pgvector + ollama）
docker compose -f deploy/docker-compose.yml up --build

# 2) PG 健康后应用迁移（幂等）
export AIDEA_DATABASE_URL=postgres://aidea:aidea@localhost:5432/aidea
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0001_init.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0002_v05.sql

# 3) 探活（gRPC HealthService）
grpc_health_probe -addr=localhost:50051
```

**单租户验证**：compose 中 `AIDEA_SINGLE_TENANT=true` + `AIDEA_TENANT_ID=single`，core 以单租户模式独立部署运行；`config.validate_tenancy()` 校验通过即可证明单租户可独立跑通（多租户隔离留待 v1.0/T11）。

### 7.5 v0.5 阶段 B 验收点勾选

- [x] **T08 真·Ollama 客户端**：`/api/generate`+`/api/embeddings` 请求构造/解析纯函数单测。
- [x] **T08 超时/退避重试**：`compute_backoff_ms`/`should_retry` 纯单测；`OllamaClient` 每次请求带超时+指数退避。
- [x] **T08 缓存**：`CompletionCache` 插入/命中/FIFO 淘汰单测；`CachedCompletionBackend` 二次调用命中缓存（计数验证）。
- [x] **T08 失败降级**：`DegradingBackend`+`RuleBasedBackend` 在真后端失败时返回规则补全（不向上抛错）。
- [x] **T08 批量并发**：`run_batch` 有界并发且保序（计数+顺序单测）。
- [x] **T08 复用 M1 测速**：`speed_test` L0 p95<300ms（mock 后端确定性通过）。
- [x] **T09 CLI 子命令**：`chat`/`craft`/`nes`/`serve`/`version` 齐备，参数解析与路由单测覆盖。
- [x] **T09 共享 Core**：CLI 直链 `ide-core`（`ChatEngine`/`CraftEngine`/`default_nes_backend`），零新重型依赖。
- [x] **T10 配置模块**：`CoreConfig` 加载/默认值/单租户开关单测；`validate_tenancy` 校验。
- [x] **T10 健康检查**：proto **仅追加** `HealthService.Check`；`health.rs` 实现并注册到 gRPC server。
- [x] **T10 私有化底座**：`docker-compose.yml`（三服务+健康+卷+环境变量）+ `Dockerfile`（多阶段）；单租户验证说明。
- [x] **纯逻辑单测可跑**：probe（client/调度/降级/缓存/退避）、cli（解析/路由）、config（加载/默认值/单租户）均 `#[test]`/`#[tokio::test]`，无外部依赖。
- [x] **M1/阶段A 契约零破坏**：proto AgentService/HostService 未改；`demo`/`serve` 行为不变；`CliHost` 行为不变；既有模块单测仍成立。

> v0.5 后续（本阶段外，已部分就绪）：Craft 多文件 Diff UI / Checkpoints 回滚可视化（T07 内核已就绪，UI 留待 Tauri 阶段）；NES 多语言/数据回流（T08 后续）；多租户隔离 RLS（v1.0/T11）。

---

## 8. v1.0 阶段 A（T11 / T14）增补

> 范围：在 M1 + v0.5 内核上**增量扩展**企业 GA 的两块底座 —— **T11 六权 RBAC 与审计**（运行时强制 + 审计落盘）与 **T14 可观测性**（结构化日志 + 指标暴露）。复用既有 `HostProvider` / `Planner` / `Llm` / `ChatEngine` / `CraftEngine` / `Permission` / `Retriever` / `Config` / `Health` 抽象，不另起炉灶。宿主仍用 `CliHost` 桩（不引入 Tauri/React 重依赖）；proto 保持 G6 冻结，admin 端点走独立 TCP，不进 `.proto`。

### 8.1 文件清单（相对 v0.5 的新增 / 修改）

| 文件 | 状态 | 职责 |
|------|------|------|
| `crates/core/src/principal.rs` | **新增** | T11 安全身份：`Principal`(tenant_id/user_id/PermissionSet)、`AuditAction`(六类动作)、`action_permission`(工具名→权限位映射)、`check_permission`(运行时强制唯一入口，复用 `permissions::require` 位运算)。 |
| `crates/core/src/audit.rs` | **新增** | T11 审计落盘：`AuditEvent`、`AuditSink` trait；`MockAuditSink`(内存，供测试/离线)、`PgAuditSink`(tokio-postgres 写 0001 `audit` 分区表，append-only)。 |
| `crates/core/src/metrics.rs` | **新增** | T14 极简指标模块：原子计数器（请求数/工具调用数/LLM 调用数/补全数/拒绝数）+ 固定桶直方图（请求延迟、补全延迟）+ `render_prometheus()` 输出 Prometheus 文本。无任何外部指标框架。 |
| `crates/core/src/admin.rs` | **新增** | T14 admin 监听：`route_admin`(纯路由判定)、`render_admin`(纯响应构造)、`serve_admin`(用 `tokio::net::TcpListener` 仅暴露 `/metrics` 与 `/healthz` 两个路由，返回文本)。 |
| `migrations/0003_v10.sql` | **新增** | 纯增量迁移（不动 0001/0002）：`log_audit` / `log_audit_event` 两个 append-only 插入函数，将 `AuditEvent` 落到 0001 的 `audit` 表并把 principal 身份写入 `detail` JSONB。 |
| `crates/core/src/config.rs` | **修改** | 新增 `admin_addr`(默认 `127.0.0.1:9090`)、`user_id`、`perm_mask` 三字段 + 对应 `AIDEA_ADMIN_ADDR`/`AIDEA_USER_ID`/`AIDEA_PERM_MASK` 环境变量；新增 `principal()` 由单租户配置构造 `Principal`。 |
| `crates/core/src/agent.rs` | **修改** | 注入 `Principal`+`AuditSink`+`Metrics`；`NesComplete`/`RunAgent`/`Chat` 三处 RPC 均在动作边界 `check_permission`，无权限返回 `PermissionDenied` 并记录拒绝审计；成功动作记录授权审计；打 `tracing` span、累加指标、统计补全延迟。 |
| `crates/core/src/planner.rs` | **修改** | 注入治理三元组；`run`/`run_with_context` 在**动作边界**（`action_permission` 命中 Read/Generate/Modify/Execute/Commit 时）强制 `check_permission`，放行/拒绝均写审计；拒绝则跳过特权动作且不改文件；`tracing` span + LLM/工具调用计数。 |
| `crates/core/src/chat.rs` | **修改** | `reply` 改用 `Principal` 校验 `Generate` 位（沿用 `PermissionSet::all()` 全权默认），授权/拒绝均写审计；`tracing` span + LLM 计数。 |
| `crates/core/src/craft.rs` | **修改** | `confirm` 改用 `Principal` 校验 Modify/Execute/Commit 位，授权/拒绝均写审计；`tracing` + 工具调用计数。 |
| `crates/core/src/tool_executor.rs` | **修改** | `BasicToolExecutor::run` 包 `tracing` span（动作执行可观测）。 |
| `crates/core/src/server.rs` | **修改** | `serve`/`serve_configured` 在起 gRPC 同时，于同进程内 `tokio::spawn` admin 监听（`serve_configured` 用 `config.admin_addr`；`serve` 用默认地址）。 |
| `crates/core/src/lib.rs` | **修改** | 暴露 `admin`/`audit`/`metrics`/`principal` 模块并 re-export 公共类型。 |
| `crates/core/Cargo.toml` | **修改** | 新增 `tokio-postgres`（仅 `runtime-tokio` + `with-serde_json-1`，无 TLS，供 `PgAuditSink`）。 |

### 8.2 各模块职责与 M1/v0.5 衔接

- **T11 六权运行时强制**：`Principal` 由 `CoreConfig::principal()` 在单租户配置下构造，贯穿 `AgentServer → Planner / ChatEngine / CraftEngine`。五个特权动作（Read/Generate/Modify/Execute/Commit）在**动作边界**统一经 `check_permission(principal, required)` 校验——这与 `permissions::require` 共用同一套位运算，位编码与 0001 的 `perm_mask` 域（0..63）逐位一致。无权限直接拒绝并写**拒绝审计**，不执行、不改文件。`Audit` 位用于标记调用方是否可读取审计（`Principal::can_read_audit`）。
- **T11 审计落盘**：所有六类动作执行后写 `AuditEvent` 到 `AuditSink`。`MockAuditSink` 落内存（单测/离线 CLI 用）；`PgAuditSink` 经 tokio-postgres 向 0001 的 `audit` 分区表做 append-only INSERT（行级触发器 + RLS 已在 0001 保证不可篡改；0003 仅追加了 `log_audit` 辅助函数）。`audit.detail` JSONB 自带 tenant_id/user_id/granted/payload，行自描述。
- **T14 结构化日志**：server/agent/planner/tool_executor/chat/craft 关键路径打 `tracing` span/event（请求进入、LLM 调用、工具执行、补全延迟）。`main.rs` 已 `tracing_subscriber::fmt::init()`；运行时以 `RUST_LOG`/`AIDEA_LOG_LEVEL` 控制级别。
- **T14 指标 + admin 端点**：`Metrics` 用原子计数器 + 固定桶直方图（锁无关），`render_prometheus()` 输出 Prometheus 文本。`serve_admin` 仅用 `tokio::net::TcpListener`（**不引入 axum/actix**）提供 `/metrics` 与 `/healthz` 两个路由；该端口独立于 gRPC ProtoBus，不进 `.proto` 契约。

### 8.3 构建与运行（同 M1 工具链，无新重型 HTTP 框架）

```bash
# 在 ide-m1/ 下（需 Rust 工具链；沙箱重型编译可能 OOM，但结构/单测逻辑正确）
cargo build                 # 编译整个 workspace（core + probe + cli）
cargo test                  # 运行全部单测（含 T11 权限/审计/MockSink、T14 metrics/路由）

# CLI 直接驱动 Core（无需先起服务）
aidea version                                   # 版本 + 配置概要（含 admin_addr / perm_mask）
aidea chat "where is main?" --attachments @file:src/main.rs
aidea craft --file mem://a.rs --old "let x = 1;" --new "let x = 2;" --kind file --yes
aidea nes --samples 50                          # NES 探针测速，断言 L0 p95 < 300ms

# 起 gRPC 服务（含 HealthService）并同进程 spawn admin 监听（默认 127.0.0.1:9090）
aidea serve "[::1]:50051"
# 另开终端抓取指标 / 健康检查（纯 TCP，文本返回）：
curl http://127.0.0.1:9090/metrics
curl http://127.0.0.1:9090/healthz
```

数据库（需 PostgreSQL 16 + pgvector 0.7，按顺序应用，0003 纯增量）：
```bash
export AIDEA_DATABASE_URL=postgres://aidea:aidea@localhost:5432/aidea
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0001_init.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0002_v05.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0003_v10.sql
```
> 单租户验证：`AIDEA_SINGLE_TENANT=true` + `AIDEA_TENANT_ID=single`，core 以单租户模式独立部署；`config.validate_tenancy()` 通过即证明可独立跑通。`PgAuditSink` 在 DB 不可达时自动降级为内存 sink（仅告警、不阻断启动）。

### 8.4 v1.0 阶段 A 验收点勾选

- [x] **T11 Principal 贯穿调用链**：`Principal`(tenant_id/user_id/perm_mask) 由 config 注入，贯穿 agent/planner/chat/craft（单测 `config::principal_mask_clamped_to_six_bits` / `admin_addr_and_principal_are_configurable`）。
- [x] **T11 动作边界强制校验**：`Read/Generate/Modify/Execute/Commit` 五类动作前调用 `check_permission(principal, required)`，无权限即拒绝并返回明确 `PermissionDenied`（单测 `planner::modify_action_denied_when_mask_lacks_modify`、`chat::reply_requires_generate_permission`、`craft::missing_modify_permission_blocks_edit`）。
- [x] **T11 审计落盘（append-only）**：所有六类动作执行后写 `AuditEvent`；`MockAuditSink` 断言条目（单测 `audit::mock_sink_records_and_counts`、`planner::modify_action_denied_*`）；`PgAuditSink` 写 0001 `audit` 分区表（0003 纯增量追加 `log_audit`/`log_audit_event`）。
- [x] **T11 掩码与 SQL 逐位一致**：`principal::mask_matches_sql_perm_has_contract`、`audit::mask_is_consistent_with_sql_domain` 验证 Rust `PermissionSet` 与 SQL `perm_mask`/`perm_has` 同编码。
- [x] **T14 结构化日志**：server/agent/planner/tool_executor/chat/craft 关键路径 `tracing` span/event（请求进入 / LLM 调用耗时 / 工具执行 / 补全延迟）。
- [x] **T14 极简指标模块**：`Metrics` 原子计数器 + 固定桶直方图 + `render_prometheus()`（单测 `metrics::counters_increment` / `histogram_buckets_and_p95` / `prometheus_render_contains_series`）。
- [x] **T14 admin 端点（无 HTTP 框架）**：`tokio::net::TcpListener` 仅暴露 `/metrics` + `/healthz`；纯函数路由判定可单测（单测 `admin::routes_resolve` / `render_produces_status_and_body` / `http_response_is_well_formed`）；由 `serve_configured` 同进程 spawn。
- [x] **纯逻辑单测可跑**：principal/audit/metrics/admin/planner 新增单测均 `#[test]`/`#[tokio::test]`，无外部依赖（除 `PgAuditSink` 走 DB 降级路径，不依赖真实 PG）。
- [x] **M1/v0.5 契约零破坏**：proto `AgentService`/`HostService`/`HealthService` 未改；`demo`/`serve`/`aidea` 行为不变；既有模块单测仍成立（qa_v05_critical 不受影响）。
- [x] **既有无重型新增依赖**：仅追加 `tokio-postgres`（轻型 PG 驱动，无 TLS）；**禁止的 axum/actix 等 HTTP 框架未引入**，admin 用原生 TCP。

> v1.0 阶段 A 后续：多租户 RLS 隔离（principal 增加 org 维度）、审计读取端点（受 `Audit` 位保护）、LDAP/OIDC 组织同步——本阶段（Stage B）已落地 **T12 SSO** 与 **T13 无界面控制台**两项；详见下文 §9。

---

## 9. v1.0 阶段 B（T12 / T13）增补

> 范围：在 M1 + v0.5 + v1.0 阶段 A 内核上**增量扩展**企业 GA 的两块 —— **T12 SSO 单点登录**（可配置认证器）与 **T13 企业控制台（无界面版）**。复用既有 `Principal` / `AuditSink` / `Metrics` / `admin` 抽象，不另起炉灶；宿主仍用 `CliHost` 桩（不引入 Tauri/React 重依赖，与一贯立场一致）。proto 保持 G6 冻结（auth 是传输层，不进 `.proto`）。

### 9.1 文件清单（相对 v1.0A 的新增 / 修改）

| 文件 | 状态 | 职责 |
|------|------|------|
| `crates/core/src/auth.rs` | **新增** | T12 SSO：`Authenticator` trait（`authenticate(token) -> Result<Principal, AuthError>`）；`NoopAuthenticator`（dev / sso 未启用，返回 config `Principal`）；`Hs256Authenticator`（共享密钥 HS256 校验 `Bearer <jwt>`，claims→`tenant_id`/`user_id`/`perm_mask`→`Principal`；密钥错/过期/缺 token 且 sso 启用/issuer·client 不匹配即 `Unauthorized`）；`extract_bearer` 传输层辅助 + JWT 签名/校验纯函数 + 单测。 |
| `crates/core/src/config.rs` | **修改** | 新增 `sso_enabled`/`sso_secret`/`sso_issuer`/`sso_client_id`（env `AIDEA_SSO_*`）；`from_map`/`from_env` 同步；`principal()` 在 sso 启用时由认证器派生（dev 兜底仍可用 Noop）。 |
| `crates/core/src/agent.rs` | **修改** | 新增 `authenticator: Arc<dyn Authenticator>`；`default_stack`/`from_config` 按 `sso_enabled` 选 Noop/Hs256；各 RPC 从 `tonic::Request::metadata()` 取 `authorization` → `extract_bearer` → `authenticate` → 派生 `Principal` → 经 `(*self.planner).clone().with_governance(...)` 注入（与 v1.0A 链路一致）；`principal()`/`metrics()` 访问器。 |
| `crates/core/src/planner.rs` | **修改** | 派生 `Clone`（供 per-request `with_governance`，所有协作者均为 `Arc`，廉价）；`record_audit` 累加 `inc_audit_events`。 |
| `crates/core/src/chat.rs` | **修改** | 派生 `Clone`；`audit` 累加 `inc_audit_events`。 |
| `crates/core/src/permissions.rs` | **修改** | 新增 `Permission::label` / `PermissionSet::from_mask` / `PermissionSet::labels`（控制台可读权限位）。 |
| `crates/core/src/metrics.rs` | **修改** | 新增 `audit_events_total` 计数 + `inc_audit_events`/`audit_events` + Prometheus 渲染（控制台"最近审计计数"来源）。 |
| `crates/core/src/admin.rs` | **修改** | T13 新增 `AdminRoute::Console` + `route_admin` 匹配 `/console`；`ConsoleStatus` / `render_console`（文本+JSON，只读）/ `ConsoleProvider` trait / `AdminConsole` 适配器；`serve_admin` 改接 `Arc<dyn ConsoleProvider>`（零 HTTP 框架）。 |
| `crates/core/src/server.rs` | **修改** | admin 监听改用 `AdminConsole::new(svc.principal(), svc.metrics())` 适配器，同进程 spawn `/metrics`+`/healthz`+`/console`。 |
| `crates/core/src/lib.rs` | **修改** | 暴露 `auth` 模块并 re-export `Authenticator`/`AuthError`/`NoopAuthenticator`/`Hs256Authenticator`。 |
| `crates/cli/src/lib.rs` | **修改** | 新增 `aidea console` 子命令（直链 Core 库构造 `ConsoleStatus` 并 `render_console` 打印）；`version` 增 SSO 状态。 |
| `crates/core/Cargo.toml` | **修改** | 新增轻量依赖 `hmac`/`sha2`/`base64`（仅此三项；**禁止** jsonwebtoken/axum/rocket 等重型库）。 |
| `Cargo.toml` | **修改** | workspace 版本升至 `1.0.0`。 |

### 9.2 各模块职责与 v1.0A 衔接

- **T12 可配置认证器**：`Authenticator` trait 是 SSO 的唯一抽象点。`NoopAuthenticator` 在 `sso_enabled=false`（默认）时忽略 token、返回 config `Principal`，因此 dev / 私有单租户部署行为完全等同于 v1.0A（`demo`/`default_stack` 照跑）。`Hs256Authenticator` 实现最小可行企业 SSO：对 `Authorization: Bearer <jwt>` 做 HMAC-SHA256 校验，从 claims 解析 `tenant_id`/`user_id`/`perm_mask`→`Principal`；密钥错/过期/缺 token（sso 启用时）/issuer·audience 不匹配一律 `AuthError::Unauthorized` → gRPC 返回 `UNAUTHENTICATED`。认证后的 `Principal` 经既有 `with_governance` 注入 Planner/ChatEngine，与 v1.0A 治理链路**逐字一致**。
- **T12 传输层位置**：auth 是传输/边界关注，不进 `.proto`（G6 冻结契约零改动）。bearer 抽取在 `agent.rs` 各 RPC 入口，纯逻辑在 `auth.rs`（可单测、无网络）。
- **T13 无界面控制台**：复用 v1.0A 的 `admin` TCP 监听，**仅新增 `/console` 路由**（与 `/metrics`、`/healthz` 并列，仍零 HTTP 框架）。`GET /console` 返回当前租户状态汇总（`tenant_id`、`user`、`perm_mask` 可读形式、最近审计计数、metrics 快照），**只读、不触发任何写入**。状态由 `ConsoleProvider` trait 抽象，`AdminConsole` 适配器用 server `Principal` + 共享 `Metrics` 构造快照。
- **T13 CLI 子命令**：`aidea console` 直链 `ide-core` 库，本地构造 `ConsoleStatus` 并打印（零 tonic 直连）；如需运行中服务的实时 metrics/审计计数，提示访问 `GET http://<admin_addr>/console`。
- **GUI 明确不实现**：高保真企业控制台 UI（Tauri/React + SVG 自绘，原 T13 计划）**本阶段不实现**，与项目一贯"不引 Tauri/React 重依赖"立场一致。本阶段的"无界面控制台"= `admin /console` + `aidea console`，GUI 留待后续阶段。

### 9.3 SSO 扩展说明（文档化，不实现）

- **OIDC RS256 / JWKS**：生产环境 IdP 多用 RS256（非对称）并通过 JWKS 端点拉取公钥。`Hs256Authenticator` 之外可再实现一个 `Rs256Authenticator`（`Authenticator` trait 即替换点），仅依赖 `rsa`/`ecdsa` + JWKS 拉取；本切片**不实现**，仅作文档预留。
- **LDAP / SAML**：组织/用户同步属 IdP 侧职责，映射到 `Principal`（含未来 `org` 维度）即可；多租户 RLS 隔离（principal 增加 org 维度）为后续 v1.0+ 任务。

### 9.4 构建与运行（同 M1 工具链，无新重型依赖）

```bash
# 在 ide-m1/ 下（需 Rust 工具链；沙箱重型编译可能 OOM，但结构/单测逻辑正确）
cargo build                 # 编译整个 workspace（core + probe + cli）
cargo test                  # 运行全部单测（含 T12 auth 各分支、T13 console 路由/状态、CLI 路由）

# CLI 直接驱动 Core（无界面控制台）
aidea version                                   # 版本 + 配置概要（含 sso_enabled）
aidea console                                   # 打印当前租户状态汇总（tenant/user/perm_mask/metrics）

# 起 gRPC 服务（含 HealthService）并同进程 spawn admin 监听（默认 127.0.0.1:9090）
aidea serve "[::1]:50051"
# 另开终端抓取指标 / 健康检查 / 控制台（纯 TCP，文本返回）：
curl http://127.0.0.1:9090/metrics
curl http://127.0.0.1:9090/healthz
curl http://127.0.0.1:9090/console
```

**启用 SSO（HS256 共享密钥）**：设置 `AIDEA_SSO_*` 环境变量后，gRPC 请求须携带 `Authorization: Bearer <jwt>`（claims 含 `tenant_id`/`user_id`/`perm_mask`，可选 `exp`/`iss`/`aud`）。例如：

```bash
export AIDEA_SSO_ENABLED=true
export AIDEA_SSO_SECRET="<shared-secret-with-idp>"
export AIDEA_SSO_ISSUER="aidea"          # 可选；留空则不校验 iss
export AIDEA_SSO_CLIENT_ID="cli"         # 可选；留空则不校验 aud
aidea serve "[::1]:50051"
# 未带合法 token 的请求返回 UNAUTHENTICATED；合法 HS256 token 派生 Principal 注入治理链。
```

> 依赖仅新增 `hmac` + `sha2` + `base64` 三项轻量库；**未引入 jsonwebtoken / axum / rocket / tonic-interceptor 等重型库**，admin 仍走原生 `tokio::net::TcpListener`。

### 9.5 v1.0 阶段 B 验收点勾选

- [x] **T12 `Authenticator` 抽象**：trait `authenticate(Option<&str>) -> Result<Principal, AuthError>`；`NoopAuthenticator`（dev/未启用忽略 token）与 `Hs256Authenticator` 均实现；`agent.rs` 持有 `Arc<dyn Authenticator>`。
- [x] **T12 HS256 校验通过/失败**：`mint_hs256_token` 签名→`verify_hs256` 校验 round-trip 通过；错误密钥/过期/缺 token→`Unauthorized`（单测覆盖）。
- [x] **T12 claims→Principal 映射**：`tenant_id`/`user_id`/`perm_mask` 正确映射；`perm_mask` 经 `Principal::from_mask` 钳制到六位；`issuer`/`client` 不匹配拒（单测）。
- [x] **T12 传输层接入 gRPC**：各 RPC 从 `metadata` 取 `authorization`→`extract_bearer`→认证；sso 未启用时 Noop 跳过（dev 兜底）；认证失败返回 `UNAUTHENTICATED`。
- [x] **T12 认证后 Principal 经 `with_governance` 注入**：`run_agent`/`chat` 用 `(*self.xxx).clone().with_governance(principal, ...)` 与 v1.0A 链路一致；无破坏 `with_defaults`/`default_stack` 向后兼容（demo 照跑）。
- [x] **T12 config 扩展**：`sso_enabled`/`sso_secret`/`sso_issuer`/`sso_client_id` + `AIDEA_SSO_*` env + `from_map`/`from_env` 同步（单测 `sso_fields_load_from_map`）。
- [x] **T13 `/console` 路由**：`route_admin` 扩展识别 `/console`；`render_admin`/`http_response` 不变（Metrics/Healthz/NotFound）；新增 `render_console` 文本+JSON（单测）。
- [x] **T13 控制台状态只读构造**：`ConsoleStatus` 汇总 tenant/user/perm_mask 可读/`audit_events`/metrics 快照；`ConsoleProvider`+`AdminConsole` 适配器；`serve_admin` 改接 `Arc<dyn ConsoleProvider>`；只读、不写。
- [x] **T13 `aidea console` CLI 子命令**：直链 Core 库构造 `ConsoleStatus` 并打印（零 tonic 直连）；路由单测覆盖。
- [x] **T13 GUI 明确不实现（文档化）**：README §9.2/§9.3 说明无界面控制台= `admin /console` + `aidea console`，高保真 UI 留待后续。
- [x] **纯逻辑单测可跑**：auth（各分支/claims/issuer/client/缺 token/过期/bearer 抽取）、console（路由/状态渲染/provider 快照）、cli（console 路由）均 `#[test]`/`#[tokio::test]`，无外部依赖（HS256 用固定 secret 的自签名测试向量 round-trip）。
- [x] **proto G6 冻结零改动**：`.proto` 未新增 RPC/消息；auth 是传输层不进契约。
- [x] **M1/v0.5/v1.0A 契约零破坏**：`demo`/`serve`/`aidea` 行为不变；既有模块单测仍成立（新增 `audit_events` 计数不干扰既有 sink 计数断言）。
- [x] **既有无重型新增依赖**：仅追加 `hmac`/`sha2`/`base64`（轻型）；**禁止的 axum/actix/jsonwebtoken/rocket 未引入**，admin 用原生 TCP。

> v1.0 后续（本阶段外）：OIDC RS256/JWKS 认证器（仅文档预留）、LDAP/SAML 组织同步、多租户 RLS 隔离（principal 增 org 维度）、审计读取端点（受 `Audit` 位保护）、高保真控制台 GUI（Tauri/React）。

---

## 10. v1.5 阶段 A（T15 / T16）增补

> 范围：在 M1 + v0.5 + v1.0 内核上**增量扩展**自主化两块 —— **T15 Quest 自主代理**（F4）与 **T16 自愈 Doom**（F3 P1）。复用既有 `Principal`/`AuditSink`/`Metrics`/`Authenticator`/`Planner`/`Llm`/`ToolExecutor`/`HostProvider`/`Validator` 抽象，不另起炉灶；宿主仍用 `CliHost` 桩（不引入 Tauri/React 重依赖）。proto **G6 冻结**：Quest/自愈均为 Core 内能力，**不进 `.proto`**；若需 gRPC 暴露 Quest，仅在 `agent.rs` 复用既有 `RunAgent`/`Chat` 入口（已落 `AgentServer::run_quest` 钩子，非新 RPC）。

### 10.1 文件清单（相对 v1.0 的新增 / 修改）

| 文件 | 状态 | 职责 |
|------|------|------|
| `crates/core/src/quest.rs` | **新增** | T15 自主代理：`Quest` / `SubTask{id,description,status}` / `SubTaskStatus` / `GoalDecomposer`(trait) / `LlmGoalDecomposer`(Llm 目标分解) / `PendingApproval` / `QuestReport{goal,subtasks,successes,failures,pending_approvals}` / `QuestConfig{quest_max_steps,quest_max_subtasks,auto_commit,max_repair_attempts,subtask_max_retries}`；`QuestToolExecutor` 审批闸（收集 Execute/Commit 待确认）；每个子任务复用既有 `Planner` 闭环（六权掩码仍强制），并包裹 `SelfHealingExecutor`。 |
| `crates/core/src/self_heal.rs` | **新增** | T16 自愈执行器：`SelfHeal`(失败→Llm 给补丁→重跑，`max_repair_attempts` 熔断) / `RepairOutcome{observation,repair_attempts,healed}` / `parse_patch`(补丁构造纯函数) / `SelfHealingExecutor`(适配器实现 `ToolExecutor` 接入 Planner)；可独立 `SelfHeal::run(tool, input, llm)`，亦被 Quest 复用。 |
| `crates/core/src/config.rs` | **修改** | 新增 `quest_auto_commit`(默认 false) / `quest_max_steps`(默认 8) / `quest_max_subtasks`(默认 8) / `max_repair_attempts`(默认 3) 四字段 + 对应 `AIDEA_*` env；新增 `quest_config()` 派生 `QuestConfig`。 |
| `crates/core/src/planner.rs` | **修改** | 新增 `tools()` / `validator()` / `bridge()` 访问器（克隆 `Arc`），供 Quest 复用既有协作者，无重复 ReAct 实现。 |
| `crates/core/src/lib.rs` | **修改** | 暴露 `quest`/`self_heal` 模块并重导出公共类型（`Quest`/`QuestConfig`/`QuestReport`/`SubTask`/`LlmGoalDecomposer`/`SelfHeal`/`SelfHealingExecutor`/`RepairOutcome` 等）。 |
| `crates/core/src/agent.rs` | **修改** | 复用既有 Planner 协作者 + 治理三元组，新增 `run_quest(goal, &QuestConfig) -> QuestReport` 钩子（非新 RPC，G6 冻结）。 |
| `crates/cli/src/lib.rs` | **修改** | 新增 `aidea quest --goal "..."` 子命令（直链 Core 库跑自主 Quest，零新重型依赖）；路由/解析单测。 |
| `README.md` | **修改** | 本节 v1.5 阶段 A 增补（验收勾选）。 |

### 10.2 各模块职责与 v1.0 衔接

- **T15 目标分解（自治 vs Craft）**：Craft 要求用户逐次确认；Quest 给定高层 `goal`，先经 `LlmGoalDecomposer`（复用 `Llm`）把 goal 拆成有序 `SubTask` 列表（`parse_subtasks` 解析编号行，纯函数可单测）。分解与执行严格分离，LLM 仅负责"想"，不触碰执行链路。
- **T15 自主 ReAct 执行循环**：每个子任务**复用既有 `Planner`**（`Planner::new(...).with_governance(principal, audit, metrics)`）跑完整 ReAct 闭环 —— 没有重写循环，证明"与既有 Planner 复用无重复 impl"。`with_governance` 注入的 `Principal` 使**六权掩码在动作边界仍强制**（与 v1.0A 链路逐字一致）。
- **T15 安全预算**：`quest_max_steps` 经 Planner 的 `BasicValidator` 复用 `Verdict::BudgetExhausted` 语义（每子任务步数上限）；`quest_max_subtasks` 在 Quest 层截断并标记 `Skipped`。任一子任务失败可重试（`subtask_max_retries`，默认 1）后跳过并标记 `Failed`。
- **T15 审批闸**：`quest_auto_commit`（默认 false）——为 false 时 `Execute`/`Commit` 类动作**不擅自执行**，改由 `QuestToolExecutor` 收集为 `PendingApproval`（返回中性 observation 让 Planner 继续）；为 true 时走 `SelfHealingExecutor` 自主执行。报告中的 `pending_approvals` 即"建议→待确认"清单（亦契合复用 Craft confirm 路径的语义）。
- **T16 自愈 Doom**：`SelfHeal` 包装工具执行；首次失败时捕获错误文本，交 `Llm` 生成修复补丁（`parse_patch` 从 `PATCH: <input>` 提取修正后输入），重跑；`max_repair_attempts` 熔断（超过即 `Err` 上报失败，不再重试）。`SelfHealingExecutor` 适配器实现 `ToolExecutor`，故既可被 Quest 在子任务执行时调用（"执行→失败→自愈→再执行"闭环），也可独立 `SelfHeal::run(tool, input, llm)` 使用。

### 10.3 构建与运行（同 M1 工具链，无新重型依赖）

```bash
# 在 ide-m1/ 下（需 Rust 工具链；沙箱重型编译可能 OOM，但结构/单测逻辑正确）
cargo build                 # 编译整个 workspace（core + probe + cli）
cargo test                  # 运行全部单测（含 T15 目标分解/预算/审批闸、T16 自愈重试/熔断）

# CLI 直接驱动 Core 跑自主 Quest（无需先起服务）
aidea quest --goal "add retry logic to utils"
AIDEA_QUEST_AUTO_COMMIT=true aidea quest --goal "implement feature X"   # 完全自主执行

# gRPC 侧如需暴露 Quest，复用既有 RunAgent/Chat 入口（AgentServer::run_quest 钩子，
# 不新增 RPC）；proto G6 冻结未改。
```

> 依赖仅新增零重型项：T15/T16 完全复用既有 `tokio`/`async-trait`/`serde_json`/`anyhow`/`tracing`，**禁止的 jsonwebtoken/axum/actix/rocket/uuid 等重型库未引入**；任务 id 沿用既有 `fast_id`（原子计数器）模式，无 uuid crate。

### 10.4 v1.5 阶段 A 验收点勾选

- [x] **T15 目标分解（mock Llm 返回固定子任务列表）**：`LlmGoalDecomposer` 经 `Llm.think` 取编号行，`parse_subtasks` 单测覆盖（`1. alpha` 解析、`goal` 兜底）；单测 `decomposition_returns_fixed_list` 用 `DecomposingLlm` 验证 3 子任务。
- [x] **T15 自主 ReAct 复用 Planner（无重复 impl）**：每子任务经 `Planner::new(...).with_governance(...)` 跑闭环；单测 `reuses_planner_for_subtask_execution` 验证 Planner 步数预算生效（子任务到 budget → Failed），证明复用而非重写。
- [x] **T15 安全预算（quest_max_steps / quest_max_subtasks）**：`quest_max_steps` 复用 `BasicValidator::BudgetExhausted`；`quest_max_subtasks` 截断并标记 `Skipped`；单测 `budget_caps_subtasks_and_skips_overflow` 验证 5 子任务→2 执行 + 3 Skipped + 2 成功。
- [x] **T15 审批闸 auto_commit=false 收集待确认**：`QuestToolExecutor` 对 `Execute`/`Commit` 类动作收集 `PendingApproval` 且不执行；单测 `auto_commit_false_collects_pending_instead_of_running` 断言工具零调用 + `pending_approvals>=1`。
- [x] **T15 审批闸 auto_commit=true 自主执行**：同闸为 true 时 `Execute`/`Commit` 经 `SelfHealingExecutor` 自主执行；单测 `auto_commit_true_runs_autonomously` 断言工具被调用 + `pending_approvals` 为空。
- [x] **T15 产出 QuestReport**：`QuestReport{goal, subtasks, successes, failures, pending_approvals}` 由 `Quest::run` 返回；CLI `aidea quest` 打印。
- [x] **T16 首次成功不触发修复**：`SelfHeal::run` 首试成功 → `repair_attempts=0`/`healed=false`；单测 `first_success_no_repair`。
- [x] **T16 首次失败→LLm 补丁→二次成功（计数 1 次修复）**：`FlakyTool` 首败、`PatchLlm` 给补丁、二次成功；单测 `fail_then_patch_then_success` 断言 `repair_attempts=1`/`healed=true`/运行 2 次。
- [x] **T16 max_repair_attempts 熔断**：始终失败时超过上限即 `Err` 并停止重试；单测 `circuit_breaker_trips` 断言 `is_err` 且总运行 `1+max_repair_attempts` 次。
- [x] **T16 修复补丁构造逻辑**：`parse_patch` 纯函数单测（`PATCH:` 前缀剥离 / 裸命令 / 空白修剪）。
- [x] **T16 与 Quest 闭环复用**：`SelfHealingExecutor` 实现 `ToolExecutor`，被 `Quest::execute_subtask` 包裹为每子任务执行器，形成"执行→失败→自愈→再执行"。
- [x] **纯逻辑单测可跑**：quest（分解/预算/审批闸/跳过）、self_heal（重试/熔断/补丁）均 `#[test]`/`#[tokio::test]`，仅 `MockLlm` + 可注入失败/成功 tool double，无外部依赖。
- [x] **proto G6 冻结零改动**：`.proto` 未新增 RPC/消息；Quest 经 `AgentServer::run_quest` 钩子复用既有 Planner，未定义新入口。
- [x] **治理链路一致**：Quest 经 `with_governance(principal, audit, metrics)` 注入，六权掩码在子任务动作边界仍强制（与 v1.0A 逐字一致）；`config` 新增四项 v1.5 开关 + `quest_config()` 派生。
- [x] **既有无重型新增依赖**：仅复用既有依赖；未引入 jsonwebtoken/axum/actix/rocket/uuid 等重型库。

---

## 11. v2.0 阶段 A（T19 / T20）增补

> 范围：在 M1 + v0.5 + v1.0 + v1.5 内核上**增量扩展**平台化两块 —— **T19 协作**（F9，无界面版）与 **T20 等保三级代码级加固**。复用既有 `Principal`/`AuditSink`/`Metrics`/`Authenticator`/`Planner`/`Llm`/`ToolExecutor`/`HostProvider`/`Validator`/`Ckg` 抽象，不另起炉灶；宿主仍用 `CliHost` 桩（不引入 Tauri/React 重依赖）。proto **G6 冻结**：协作/租户/合规均为传输层或管理面/文档，不进 `.proto`（如暴露走 admin TCP 或 CLI）。

### 11.1 文件清单（相对 v1.5 的新增 / 修改）

| 文件 | 状态 | 职责 |
|------|------|------|
| `crates/core/src/collab.rs` | **新增** | T19 协作数据层：`Comment`/`CommentStore`(`InMemoryCommentStore`/`PgCommentStore`)、`Lock`/`LockStore`(`InMemoryLockStore`)、`SET_TENANT_LOCAL_SQL` 租户会话注入常量。 |
| `crates/core/src/security.rs` | **新增** | T20 静态加密：`PgSecretStore` 封装 `0005` 的 `set_secret`/`get_secret`（pgcrypto），`SET_TENANT_LOCAL_SQL` 常量 + 对迁移 SQL 的静态自洽测试。 |
| `crates/core/src/audit.rs` | **修改** | T20：`AuditEvent::row_hash` 计算 SHA-256 防篡改链；`PgAuditSink::connect` 注入会话租户、`record` 写入 `tenant_id`/`prev_hash`/`row_hash`。 |
| `crates/core/src/ckg.rs` | **修改** | T20：`PgCkgStore::connect` 注入会话租户；符号/边持久化写入 `tenant_id`。 |
| `crates/core/src/config.rs` | **修改** | T20 新增 `enc_key`（`AIDEA_ENC_KEY`，DEK，内存持有不落库）。 |
| `crates/core/src/lib.rs` | **修改** | 暴露 `collab`/`security` 模块并重导出公共类型。 |
| `crates/core/src/admin.rs` | **修改** | O2 非阻塞遗留：`Content-Type` 补 `charset=utf-8`。 |
| `crates/core/src/agent.rs` | **修改** | `PgAuditSink::connect` 调用同步新增 `tenant_id` 参数。 |
| `crates/cli/src/lib.rs` | **修改** | 新增 `aidea comment`/`aidea lock`/`aidea secret` 子命令（直链 Core 库，零 tonic）+ 路由/解析单测。 |
| `migrations/0005_v20.sql` | **新增** | 纯增量迁移：RLS（7 表 + tenant_id 列 + 策略）、pgcrypto `secrets` 表 + 加解密函数、审计防篡改链（`prev_hash`/`row_hash` + 触发器 + `audit_verify_chain()`）。 |
| `docs/compliance-mapping.md` | **新增** | T20 等保三级控制项 ↔ 实现落点映射；明确标注「代码级加固已完成，真实测评需另行委托」。 |

### 11.2 各模块职责与 v1.5 衔接

- **T19 协作（无界面）**：`collab.rs` 提供代码评审/共享标注 `Comment`（锚定 `(file, line_range)`，租户隔离）与编辑 presence 提示 `Lock`（内存即可）。`CommentStore` 抽象下 `InMemoryCommentStore`（测试/离线）+ `PgCommentStore`（tokio-postgres，连 PG 后 `SET LOCAL app.tenant_id` 注入会话租户，复用既有连接模式）。协作**不接入** `with_governance` 链路（非特权动作），仅复用 `tenant_id` 概念，与 `Principal` 租户模型一致。CLI `aidea comment {list,add,resolve}` / `aidea lock {acquire,release,show}` 直链 Core 库。
- **T20 等保三级（代码级）**：
  1. **RLS 行级安全**：`0005` 为 `audit`/`context_sources`/`ckg_symbols`/`ckg_edges`/`comments`/`locks`/`secrets` 追加 `tenant_id TEXT NOT NULL DEFAULT 'default'` + 索引；`ENABLE ROW LEVEL SECURITY` + `CREATE POLICY ... USING (tenant_id = current_setting('app.tenant_id', true))`（用 `coalesce` 保证未设会话租户时向后兼容）。`audit` 用 **RESTRICTIVE** 策略与既有 `audit_append_only`  permissive 策略 AND 组合，隔离不破坏只追加。`PgAuditSink`/`PgCkgStore`/`PgCommentStore`/`PgSecretStore` 连 PG 后 `SET app.tenant_id`（单租户默认 `'default'`，多租户就绪）。
  2. **pgcrypto 静态加密**：新建 `secrets(tenant_id, name, value_encrypted bytea)` + `set_secret`/`get_secret`（`pgp_sym_encrypt`/`pgp_sym_decrypt`，key 取自 `AIDEA_ENC_KEY`）；通用 `pg_encrypt_text`/`pg_decrypt_text` 供敏感列（如 `audit.detail_encrypted` 可选加密镜像）使用。`pgcrypto` 扩展在 `0001` 已 `CREATE EXTENSION`。
  3. **审计不可变**：`audit` 表加 `prev_hash`/`row_hash` 列 + BEFORE INSERT 触发器 `audit_chain_before_insert`（计算 `row_hash = sha256(action|perm_bit|tenant_id|prev_hash)`，`prev_hash` 取同租户上一行 hash）；`audit_verify_chain()` 验证查询检出篡改/断链。Rust 侧 `audit.rs` 的 `PgAuditSink` 写入时计算并写入 `row_hash`（纯增量适配，不影响 `MockAuditSink`）。
  4. **合规映射文档**：`docs/compliance-mapping.md` 逐条映射等保三级控制项（访问控制/安全审计/数据完整性/数据保密性/通信加密/剩余信息保护）↔ 本实现落点，并明确标注「代码级加固已完成，真实等保三级测评认证需另行委托」。
  5. **配置**：`config.enc_key`（`AIDEA_ENC_KEY`）；`tenant_id` 沿用既有配置（`single` 默认）。

### 11.3 构建与运行（同 M1 工具链，无新重型依赖）

```bash
# 在 ide-m1/ 下（需 Rust 工具链；沙箱重型编译可能 OOM，但结构/单测逻辑正确）
cargo build                 # 编译整个 workspace（core + probe + cli）
cargo test                  # 运行全部单测（含 T19 评论/锁/租户隔离、T20 RLS/防篡改链/secrets SQL 断言）

# CLI 协作（无需先起服务，直链 Core 库）
aidea comment list src/main.rs
aidea comment add src/main.rs 10 --text "guard this with a mutex"
aidea comment resolve <id>
aidea lock acquire src/main.rs      # 提示"某人正在编辑"
aidea lock show src/main.rs

# CLI 静态加密（需可达 Postgres + AIDEA_ENC_KEY）
export AIDEA_ENC_KEY="super-secret-dek"
aidea secret set api_key "s3cr3t"   # pgcrypto 加密落库
aidea secret get api_key            # 解密读回
```

数据库（需 PostgreSQL 16 + pgvector 0.7，按顺序应用，0005 纯增量）：
```bash
export AIDEA_DATABASE_URL=postgres://aidea:aidea@localhost:5432/aidea
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0001_init.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0002_v05.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0003_v10.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0004_v15.sql
psql "$AIDEA_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/0005_v20.sql

# 启用 RLS + 审计防篡改链后，验证审计完整性：
psql "$AIDEA_DATABASE_URL" -c "SELECT * FROM audit_verify_chain();"   # 空结果 = 链完整
```

> **启用租户隔离（多租户就绪）**：为连接设置会话租户即可，例如 `SET app.tenant_id = 'acme';`（Core 的 Pg* store 在 connect 后自动设置）。单租户下默认租户即生效，RLS 不阻断现有读写。

### 11.4 v2.0 阶段 A 验收点勾选

- [x] **T19 `Comment` 增/查/解决（内存 store）**：`InMemoryCommentStore` 单测覆盖 add/list/resolve。
- [x] **T19 租户隔离**：`list_for_file` 仅返回本 tenant；跨 tenant 不可见/不可 resolve（单测）。
- [x] **T19 `LockStore` 在线编辑提示**：`InMemoryLockStore` acquire/release/get + 跨租户隔离（单测）。
- [x] **T19 `aidea comment` / `aidea lock` CLI 路由**：list/add/resolve/acquire/release/show 解析 + 路由单测。
- [x] **T20 RLS 策略 SQL 静态自洽**：`0005` 对 7 表 `ENABLE ROW LEVEL SECURITY` + 策略引用 `current_setting('app.tenant_id')`（SQL 文本断言）。
- [x] **T20 审计防篡改链存在**：`audit_verify_chain`/`audit_row_hash`/`audit_chain_before_insert` + `prev_hash`/`row_hash` 列均定义；Rust `AuditEvent::row_hash` 确定性 + 链式单测。
- [x] **T20 secrets 加解密 round-trip**：`set_secret`/`get_secret` + `pgp_sym_encrypt`/`pgp_sym_decrypt` 定义（SQL 文本断言 + Rust 持有 `enc_key` 流程说明，不连真 PG）。
- [x] **T20 tenant 会话注入点**：`PgAuditSink`/`PgCkgStore`/`PgCommentStore`/`PgSecretStore` 连 PG 后 `SET app.tenant_id`；`SET LOCAL` 用于每事务严格隔离（常量可测）。
- [x] **T20 `config.enc_key` + `AIDEA_ENC_KEY`**：加载/默认空（dev）单测；密钥内存持有不落库。
- [x] **T20 合规映射文档**：`docs/compliance-mapping.md` 逐条映射 + 明确「代码级加固完成，真实测评需另行委托」。
- [x] **纯逻辑单测可跑**：collab（增/查/解决/租户隔离/锁）、security（SQL 文本断言/常量）、audit（row_hash 链）均 `#[test]`/`#[tokio::test]`，无外部依赖。
- [x] **proto G6 冻结零改动**：`.proto` 未新增 RPC/消息；协作/租户/合规不进契约（走 CLI / admin TCP / 文档）。
- [x] **M1..v1.5 契约零破坏**：`demo`/`serve`/`aidea` 行为不变；既有模块单测仍成立（O1 审计双重写入未强制改，O2 admin charset 顺手补）。
- [x] **既有无重型新增依赖**：仅复用既有依赖（tokio-postgres/hmac/sha2/base64/serde_json）；**未引入 axum/actix/uuid/sea-orm 等重型库**。

> 后续（本阶段外）：真实等保三级测评认证（委托 authorized assessor）、TLS 终止/PG sslmode、KMS 托管 `AIDEA_ENC_KEY`、OIDC RS256/JWKS 与多租户端到端渗透测试（已在 v1.0B 文档预留）。

---

## 12. v2.0 阶段 B — T21 测试评测收口

> 阶段 B（T21）在 **M1→v2.0A 已落地代码之上做增量**，目标是把分散在模块内 `#[test]`/`#[tokio::test]` 的单测与既有集成测试（`tests/qa_v05_critical.rs`、`tests/qa_v10_stage_a.rs`、`tests/qa_v10_stage_b.rs`）**整合为可一键跑的完整套件**，并补齐一套**进程内端到端 eval 场景**（`crates/core/tests/eval.rs`，无外部依赖、`mock` 设施复用），最终沉淀**测试/质量清单**（`docs/test-report.md`）并**收口已知非阻断项**。
>
> 设计约束（与 v2.0A 一致并继承）：
> - **proto G6 冻结**：eval 不在 `.proto` 新增任何 RPC / 消息；评测全部走进程内集成，复用 `MockLlm` / `CliHost` / `InMemoryCkg` / `InMemoryCommentStore` / `MockAuditSink` 等既有设施，**不重写生产代码、不另起炉灶**。
> - **零重型新增依赖**：eval 仅复用既有 dev 依赖（async-trait / tokio test-util / serde_json），**未引入 criterion / rstest / 外部 mock 框架 / axum / actix / jsonwebtoken / rocket**。
> - **不强制改 O1 生产逻辑**：审计「双重写入」（RPC 级 + 业务级）属既有设计取舍，eval 场景 7 用 `>=` 容差收纳，避免回归；仅在评测/文档层收口。

### 12.1 测试套件结构

全量测试通过 `cargo test` 聚合，分三层：

| 层级 | 位置 | 内容 | 是否需外部依赖 |
| --- | --- | --- | --- |
| 模块内单测 | `crates/core/src/**` 各 `mod tests` | planner / ckg / collab / security / audit / metrics / auth / chat / craft / quest / self_heal / principal 等纯逻辑单测 | 否 |
| 既有集成测试 | `crates/core/tests/{qa_v05_critical,qa_v10_stage_a,qa_v10_stage_b}.rs` | TG1–TG6、TG-B1–TG-B6、craft/chat 关键路径、Hs256 鉴权链路 | 否 |
| 阶段 B eval 套件 | `crates/core/tests/eval.rs`（新增） | T21 八场景端到端集成（见 §12.2） | 否 |

> 运行：
> ```bash
> cargo test -p ide-core      # 覆盖全部模块单测 + 三类集成测试（含 eval）
> cargo test -p ide-cli       # CLI 路由 / 解析单测
> cargo test -p ide-probe     # probe 单测
> cargo test                 # 全 workspace 聚合
> ```

### 12.2 T21 eval 八场景职责

`crates/core/tests/eval.rs` 以进程内集成方式串联既有组件，每个场景对应用户验收点：

| # | 场景函数 | 串联组件 | 断言要点 |
| --- | --- | --- | --- |
| 1 | `demo_react_completes` | `CliHost` + `MockLlm` + `Planner::with_defaults` | 终端态 `BudgetExhausted`/终止；首条 observation 以 `"inspected"` 起头 |
| 2 | `chat_returns_answer_and_suggestion` | `ChatEngine` + `MockLlm` | 返回 finish + tool；最终 answer 非空 + 含 tool suggestion |
| 3 | `craft_propose_then_confirm_applies_edit_under_permission` | `CraftEngine` + `CliHost`（双读） | `HostProvider::apply_edit` 被调用且文件内容变更；无 Modify 权限则 confirm 被拒 |
| 4 | `quest_decomposes_and_collects_pending_when_auto_commit_false` | `Quest` + `MockLlm` 分解 | `auto_commit=false` → `QuestReport.pending_approvals` 非空，且不自主任意 commit |
| 5 | `self_heal_retries_on_failure` | `SelfHeal` + 首失败 `FlakyTool` + `MockLlm` patch | 第二次成功，`repair_attempts == 1`，`healed == true` |
| 6 | `ckg_query_returns_neighborhood` | `InMemoryCkg` 注入样例 | `query(name)` 返回关联符号（Calls / Contains 关系） |
| 7 | `audit_records_and_metrics_increment` | 动作链 + `MockAuditSink` + `Metrics` | sink 事件数自增；`metrics.audit_events()` 用 `>=` 容差收纳 O1 双重写入 |
| 8 | `sso_no_token_denies_when_enabled` | `Hs256Authenticator`（启用）/ `NoopAuthenticator`（兜底） | 启用但无 token → `Unauthorized`；Noop 兜底不拒绝 |

> eval 仅引入轻量 **测试替身**（`ModifyActionLlm` / `CommitQuestLlm` / `PatchLlm` / `FlakyTool`），均为既有 trait 的最小实现，不触碰生产代码。

### 12.3 已知非阻断项现状（阶段 B 收口）

| 编号 | 项 | 性质 | 现状 / 处置 |
| --- | --- | --- | --- |
| **O1** | 审计「双重写入」（RPC + 业务级计数） | 既有设计取舍 | **不强制改生产逻辑**；eval 场景 7 用 `>=` 容差收纳，避免回归。文档层已标注为 design choice。 |
| **O2** | admin Content-Type charset | 已在 v2.0A 修复 | 已修；阶段 B 复核无残留。 |
| **O3** | 命名差（`default_stack` / `with_defaults` vs 文档） | 向后兼容 | 保留别名，向后兼容；文档层说明，不强制改名。 |
| **Sandbox OOM** | 沙箱内存上限导致重型依赖编译失败 | 环境限制 | 代码静态自洽、依赖声明合理（零新增重型依赖）；本地 / CI 内存充足时可点亮 `cargo test`。 |
| **GUI 控制台** | 未实现图形控制台 | 超出阶段 B 范围 | 按既定 scope 不在本期；CLI / 集成测试覆盖等价能力。 |
| **知识库 9 个冗余归档** | 非代码资产冗余 | 非代码 | 用户侧清理，不影响编译 / 评测。 |

### 12.4 v2.0 阶段 B 验收点勾选

- [x] **T21 eval 套件落地**：`crates/core/tests/eval.rs` 八场景全部以进程内集成实现，无外部依赖、复用既有 mock 设施。
- [x] **既有单测整合为可跑套件**：模块内单测 + `tests/qa_*.rs` 三类集成测试 + eval 经 `cargo test -p ide-core` 一键聚合。
- [x] **不重写生产代码**：eval 仅新增测试文件与轻量测试替身；生产 crate 零改动、依赖零新增。
- [x] **proto G6 冻结零改动**：`.proto` 未新增任何 RPC / 消息；评测不进契约。
- [x] **测试/质量清单产出**：`docs/test-report.md` 含全量测试聚合方式、分模块清单、累计单测数估算、八场景职责表、已知非阻断项登记、全量验收勾选。
- [x] **已知非阻断项收口**：O1（eval `>=` 容差，不强制改生产）/ O2（已修）/ O3（向后兼容）/ Sandbox OOM（环境）/ GUI（scope）/ 知识库冗余（用户侧）均已登记并给出处置。
- [x] **README §12 收口**：测试套件结构 + 质量清单摘要 + 已知项现状 + 全量验收勾选已增补。
- [x] **无 QA 回归**：既有 `qa_v05_critical` / `qa_v10_stage_a` / `qa_v10_stage_b` 与模块单测未被破坏；eval 为纯新增。
- [x] **累计单测数估算**：workspace 约 **121**（core ≈97 / cli ≈14 / probe ≈10），详见 `docs/test-report.md` §3。

> 阶段 B 交付物：`crates/core/tests/eval.rs`（新增）、`docs/test-report.md`（新增）、`README.md` §12（新增）。生产代码与 `.proto` 零改动。
