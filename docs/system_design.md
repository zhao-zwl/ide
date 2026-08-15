# aidea macOS GUI 客户端（自包含 .dmg）— 最终系统架构设计 + 任务分解

> 作者：架构师 高见远（software-architect）
> 输入：增量 PRD（许清楚）+ 8 项已确认决策（用户拍板）+ `ide-m1` 代码核查
> 交付：最终架构设计（Part A）+ 任务分解（Part B）
> 形态变更：**单一 .dmg 自包含**——GUI + Ollama 运行时 + 端模型 qwen2.5:0.5b + PostgreSQL + aidea serve 一体；安装后 GUI 自动拉起后端栈，零配置对话；设置可切在线厂商（OpenAI 兼容）。

---

## Part A：系统设计方案

### 1. 实现方案 + 框架选型

#### 1.1 技术栈与形态

- **Tauri v2**：Rust 后端（WebView 桥接）+ 前端 React/Vite。最终 `cargo tauri build` 产出 `.app` → `.dmg`，ad-hoc 签名，预留 notarization 扩展点。
- **gRPC 连通**：Tauri Rust 端用 **tonic 0.11 客户端**直调本地 `aidea serve`（HTTP/2，`127.0.0.1:50051`）。proto 复用 `ide-core` 已生成的 `ide_core::v1`（GUI 作为 workspace 成员依赖 `ide-core`，**不重复 build proto**）。
- **自包含后端栈（核心变更）**：GUI 负责拉起本地后端栈——PostgreSQL（bundled 二进制 + 自建数据目录）、Ollama（bundled 二进制 + 端模型）、`aidea serve`（bundled `aidea` 二进制，GUI 以 sidecar 启动并管理生命周期）。**用户无需手动起任何服务。**
- **模型后端（local/online）**：GUI 始终走本地 `aidea serve` gRPC；本地/在线切换由 **aidea serve 后端路由**。在线模式 API Key 由后端持有（GUI 仅在配置时从 keyring 临时读取做连通测试，落地时由 serve 进程持有，前端不持久化/不日志）。

#### 1.2 关键难点与核心决策（含代码核查结论）

| 难点 | 代码核查结论 | 决策 |
|------|--------------|------|
| **在线厂商路由是否存在** | ❌ 不存在。`crates/probe/src/ollama.rs` 的 `CompletionBackend` 仅 `OllamaClient`+Mock；`crates/core/src/llm.rs` 的 `Llm` 仅 `MockLlm`，**无 OpenAI 实现**；grep 全仓无 `openai`/`chat/completions` 源码。 | **需新增服务端「LLM backend 模块」**（OllamaLlm + OpenAiLlm + OpenAI CompletionBackend + 配置选择）。属服务端改动，列为待明确 #A，但确属必需。 |
| **本地真实 chat/quest** | ❌ 当前 `Planner::with_defaults` 硬编码 `MockLlm`，Chat/Quest 永远走 Mock。**即便 Ollama+模型就绪，也无 OllamaLlm 实现**。 | 同上模块一并新增 `OllamaLlm`（调用 Ollama `/api/chat` 或 `/api/generate`，模型 `nes-tab:latest`）。这样才能兑现「端模型零配置对话」。 |
| **Key 如何传给 serve（不改 gRPC）** | — | GUI **拥有 serve 生命周期**，在 `aidea serve` 启动命令的 **环境变量**注入 `AIDEA_LLM_BACKEND/API_KEY/BASE_URL`（key 来自 keyring）。**无需新增 gRPC RPC 传 key**。运行时切换 = 重启 serve 进程（GUI 管理）。 |
| **PG 自包含** | `aidea serve` **不自动建库/不跑迁移**（grep 无 migrate；`crates/core/src/main.rs` 仅 `server::serve`）。 | GUI bootstrap 负责：`initdb` → 启动 PG → `createdb aidea` + 建 role → 按顺序应用 `migrations/*.sql`。与系统 PG（`/usr/local/var/postgres-17`）**隔离**，用 AppData 内独立数据目录。 |
| **Ollama + 模型 bundle** | 无现成打包逻辑（全新）。 | bundle Ollama 二进制 + `qwen2.5:0.5b` 权重到 .app；首次启动 `ollama create nes-tab -f Modelfile`（FROM qwen2.5:0.5b）对齐 `model_name` 默认值 `nes-tab:latest`。 |
| **Quest/Craft/Comment/Lock/Secret 无 gRPC RPC** | （前次核查）proto 仅 `AgentService.{Ping,NesComplete,RunAgent,Chat}` + `HealthService.Check`；`run_quest` 是私有方法。 | 沿用前次方案：GUI 进程内**复用 ide-core 库**（与 CLI 同构），gRPC 仅用于 Chat/RunAgent/NesComplete/Health。无需改服务端。 |
| **多 sidecar 编排** | 全新。 | Rust `bootstrap` 模块用 `std::process::Command` 拉起并监管 sidecar，按 PG→Ollama→serve 顺序，健康检查 + 失败重试（见 §4、§8）。 |

#### 1.3 架构模式与目录结构

- Rust 端分层：`commands/`（Tauri 命令）→ `bootstrap/`（sidecar 编排）+ `grpc/`（gRPC 客户端/流式）+ `domain/`（进程内 ide-core 复用）+ `model_backend.rs`（vendor 配置→env）+ `state.rs`。
- 前端：React + `zustand`；`invoke` + `listen` 通信；新增 `BootstrapPage`（首次拉起进度）、`SettingsPage`（模型后端/在线厂商）。
- **目录结构（新增/调整，置于 `ide-m1/gui/`）**：

```
ide-m1/                          (cargo workspace)
├── Cargo.toml                   ← members 追加 "gui/src-tauri"
└── gui/
    ├── package.json / index.html / vite.config.ts / tsconfig*.json
    ├── tailwind.config.js / postcss.config.js
    ├── scripts/
    │   ├── build-dmg.sh          (拷贝 sidecar 二进制 + 权重 + ad-hoc 签名 + 打 dmg)
    │   └── fetch-binaries.sh     (开发期拉取/构建 aidea、pg、ollama 到 resources)
    ├── src/                      (前端，见 §2)
    └── src-tauri/
        ├── Cargo.toml            (ide-core path、tauri v2、tonic、tokio、serde、keyring、tauri-plugin-store、tauri-plugin-shell、reqwest、thiserror、anyhow)
        ├── build.rs
        ├── tauri.conf.json       (externalBin/资源声明、bundle 标识、macOS 目标)
        ├── capabilities/default.json
        ├── resources/
        │   ├── migrations/        (拷贝 migrations/*.sql，bootstrap 应用)
        │   ├── models/nes-tab/    (Modelfile + qwen2.5:0.5b 权重 blobs)
        │   └── pg/                (bundled pg 二进制：initdb/postgres/pg_ctl/psql/lib)
        └── src/
            ├── main.rs
            ├── error.rs / state.rs / config.rs
            ├── model_backend.rs   (vendor 配置 → serve 启动 env)
            ├── grpc/{mod,client,stream}.rs
            ├── domain/{mod,core_config,quest,craft,collab}.rs
            ├── bootstrap/{mod,sidecar,pg,ollama,serve}.rs   (★新增 sidecar 编排)
            └── commands/{mod,connection,chat,agent,quest,craft,collab,health,model}.rs  (+model)
```

---

### 2. 文件列表（相对路径，含 sidecar/打包）

#### Rust 端（`gui/src-tauri/`）
| 文件 | 作用 |
|------|------|
| `Cargo.toml` | 依赖声明（见 §6） |
| `build.rs` | Tauri 构建钩子（不重复 build proto） |
| `tauri.conf.json` | sidecar/外部二进制与资源声明、bundle 标识、ad-hoc 签名占位、notarization 扩展点 |
| `capabilities/default.json` | 命令/事件权限白名单 |
| `resources/migrations/*.sql` | 拷贝自 `ide-m1/migrations/`，bootstrap 顺序应用 |
| `resources/models/nes-tab/Modelfile` | `FROM qwen2.5:0.5b`（对齐 `nes-tab:latest`） |
| `resources/models/nes-tab/*.bin` | qwen2.5:0.5b 权重（离线零配置） |
| `resources/pg/{initdb,postgres,pg_ctl,psql,...}` | bundled PostgreSQL 17 二进制与 lib |
| `src/main.rs` | 入口：注册插件 + 全部 commands + `AppState` + 启动 `bootstrap` |
| `src/error.rs` | `GuiError{code,message}` + `From` |
| `src/state.rs` | `AppState`：gRPC 客户端、连接状态、abort map、`Store`、bootstrap 句柄 |
| `src/config.rs` | 非密钥配置（vendor kind、base_url、auto_bootstrap）+ keyring 读密钥 |
| `src/model_backend.rs` | `VendorConfig{ kind, base_url?, api_key?(keyring), local_model }` → 构造 serve 启动 env |
| `src/grpc/mod.rs` `client.rs` `stream.rs` | gRPC 客户端封装 + Chat/RunAgent 流式→事件 |
| `src/domain/{mod,core_config,quest,craft,collab}.rs` | 进程内复用 ide-core（quest/craft/comment/lock/secret） |
| `src/bootstrap/mod.rs` | 编排入口：`bootstrap_stack()` 状态机 + 重试 |
| `src/bootstrap/sidecar.rs` | `spawn_sidecar(name,args,env)` + 监管/停止 |
| `src/bootstrap/pg.rs` | `initdb`(若缺) → 启动 PG → `createdb aidea` + role → 应用 migrations |
| `src/bootstrap/ollama.rs` | 启动 Ollama → `ollama create nes-tab`（FROM 本地 qwen2.5:0.5b） |
| `src/bootstrap/serve.rs` | 以 `model_backend` 的 env 启动 `aidea serve 127.0.0.1:50051` + 轮询 `Health.Check` |
| `src/commands/{mod,connection,chat,agent,quest,craft,collab,health,model}.rs` | Tauri 命令 |

#### 前端（`gui/src/`）
| 文件 | 作用 |
|------|------|
| `main.tsx` `App.tsx` `styles/index.css` | 入口/根/样式 |
| `api/invoke.ts` `types/models.ts` | invoke 封装 + 类型 |
| `store/{connection,chat,quest,craft,collab,health,model}.ts` | 状态（model 含 vendor 配置） |
| `components/{Layout,Sidebar,ErrorToast,BootstrapProgress}.tsx` | 布局/导航/错误/首次拉起进度 |
| `pages/{BootstrapPage,SettingsPage,ChatPage,QuestPage,CraftPage,CollabPage,HealthPage}.tsx` | 页面（连接页并入 Bootstrap/Settings；新增设置「模型后端」） |

#### 打包脚本（`gui/scripts/`）
| 文件 | 作用 |
|------|------|
| `fetch-binaries.sh` | 构建/拷贝 `aidea`、`pg` 二进制、`ollama`、模型权重到 `resources/` |
| `build-dmg.sh` | 汇编 .app 资源、ad-hoc 签名、产出 .dmg；预留 `NOTARIZE=1` 开关 |

---

### 3. 数据结构和接口

#### 3.1 Tauri Command 接口（前端 ↔ Rust，更新：模型后端/在线厂商/连接状态）

| Command | 入参 | 返回 | 后端 | 说明 |
|---------|------|------|------|------|
| `bootstrap_status` | — | `BootstrapState` | `bootstrap` | 后端栈拉起进度/阶段 |
| `get_vendor_config` | — | `VendorConfig` | `model_backend` | 当前模型后端 |
| `set_vendor_config` | `VendorConfig` | `void` | `model_backend` | 写 keyring + **重启 serve**（切换 local/online） |
| `test_vendor` | `{ base_url, api_key?(临时) }` | `bool` | `model_backend` | GUI 临时读 key 探活 OpenAI 兼容 `/v1/chat/completions` |
| `connect_status` | — | `ConnStatus` | `state` | 连接状态（自动托管，localhost:50051） |
| `chat_send` / `chat_stop` | `{session_id,message,attachments}` / `{session_id}` | `void` | `chat`+`grpc/stream` | 流式（事件 `chat-token/done/error`） |
| `agent_run` | `{session_id,goal,project_id}` | `void` | `agent`+`grpc/stream` | RunAgent 流式 |
| `quest_run` | `{goal,auto_commit?}` | `QuestReport` | `domain/quest` | 进程内 ide-core |
| `craft_propose` / `craft_confirm` / `craft_reject` | … | `CraftProposal`/`CraftState` | `domain/craft` | 进程内 |
| `comment_*` / `lock_*` / `secret_*` | … | … | `domain/collab` | 进程内（comment/secret 直连 bundled PG） |
| `fetch_health` / `fetch_console` / `fetch_metrics` | — | `HealthOverview`/`ConsoleStatus`/`string` | `health` | 拉 9090 |

> `ConnStatus` 简化为 `booting | connected | error`；gRPC 地址固定 `127.0.0.1:50051`，前端不可手填（由 GUI 拉起）。

#### 3.2 Rust 端 ↔ aidea gRPC 映射表（不变部分）

| 能力 | gRPC RPC | 备注 |
|------|----------|------|
| 连接/健康 | `HealthService.Check` | 探活 + bootstrap 轮询 |
| Chat 流式 | `AgentService.Chat` | 多轮靠 `session_id` |
| Agent(ReAct) | `AgentService.RunAgent` | 可选 |
| NES | `AgentService.NesComplete` | 可选 |
| Quest/Craft/Comment/Lock/Secret | —（无 RPC） | 进程内复用 ide-core |

#### 3.3 前端状态模型（关键 TS 类型，`types/models.ts`）

```ts
type BootstrapPhase = "idle"|"pg"|"ollama"|"model"|"serve"|"ready"|"error";
interface BootstrapState { phase: BootstrapPhase; progress: number; detail?: string; }
type ConnStatus = "booting"|"connected"|"error";
interface VendorConfig { kind: "local"|"online"; base_url?: string; local_model: string; /* api_key 不落前端 */ }
interface ChatMessage { role:"user"|"assistant"|"system"; content:string; }
interface QuestReport { goal:string; subtasks:SubTask[]; successes:number; failures:number; pending_approvals:PendingApproval[]; }
interface SubTask { id:string; description:string; status:"pending"|"running"|"success"|"failed"|"skipped"; }
interface PendingApproval { id:string; tool:string; argument:string; subtask_id:string; }
interface CraftProposal { id:string; document_uri:string; old_text:string; new_text:string; rationale:string; kind:"FileEdit"|"RunCommand"|"Commit"; state:"Suggestion"|"PendingConfirm"|"Applied"|"Rejected"; }
interface Comment { id:string; tenant_id:string; file:string; line_start:number; line_end:number; author:string; body:string; resolved:boolean; created_at:number; }
interface Lock { tenant_id:string; file:string; owner:string; acquired_at:number; }
interface ConsoleStatus { tenant_id:string; user_id:string; perm_mask:number; permissions:string; audit_events:number; metrics:{requests:number;tool_calls:number;llm_calls:number;completions:number;denials:number;request_p95_ms:number;}; }
interface HealthOverview { healthz:string; console:ConsoleStatus; }
```

#### 3.4 类图（见 `docs/class-diagram.mermaid`）

要点：`AppState` 聚合 `BootstrapHandles`（各 sidecar Child + 取消）、gRPC 客户端、vendor 配置；`bootstrap/*` 模块编排 sidecar；`model_backend.rs` 把 `VendorConfig` 翻译成 serve 启动 env；`commands/model.rs` 暴露 vendor 切换/测试。

---

### 4. 程序调用流程（Mermaid，见 `docs/sequence-diagram.mermaid`）

覆盖 6 条主线：
1. **首次启动拉起后端栈**（PG→Ollama→模型→serve→health 转绿）含失败重试。
2. **Chat（本地）**：GUI→serve(gRPC Chat)→Ollama(`nes-tab`)→流式回显。
3. **Chat（在线）**：GUI→serve(gRPC Chat)→serve 持 Key 调 OpenAI 兼容 `/v1/chat/completions`→流式回显（前端不接触 Key）。
4. **模型后端切换**：Settings→`set_vendor_config`→停旧 serve→以新 env 重启 serve→health 转绿。
5. **Quest / Comment / Lock / Secret**：进程内 ide-core（直连 bundled PG）。
6. **健康概览**：拉 9090 `/healthz`+`/console`。

---

### 5. 依赖包列表

#### Rust（`gui/src-tauri/Cargo.toml`）
```
tauri = { version = "2" }
tauri-plugin-store = "2"          # 非密钥配置
tauri-plugin-shell = "2"          # sidecar 启动（亦可用 std::process::Command）
serde = { version="1", features=["derive"] }
serde_json = "1"
tokio = { version="1", features=["full"] }
tonic = "0.11"                    # gRPC 客户端（与服务端同版本）
prost = "0.12"
futures-util = "0.3"
tokio-util = "0.7"                # CancellationToken（停止生成）
ide-core = { path = "../../crates/core" }   # 复用生成 gRPC 客户端 + 领域逻辑
keyring = "3"                     # macOS Keychain 存 API Key / DB 密码
reqwest = { version="0.12", features=["json"] }  # 在线厂商连通测试（GUI 侧探活）
thiserror = "1"  anyhow = "1"
# build: tauri-build = "2"
```

#### 前端（`gui/package.json`）
```
react ^18 / react-dom ^18
@tauri-apps/api ^2  @tauri-apps/plugin-store ^2
vite ^5  @vitejs/plugin-react ^4  typescript ^5
@mui/material ^5 + @emotion/react + @emotion/styled
tailwindcss ^3 + postcss + autoprefixer
zustand ^4
# （可选）tauri-specta：Rust→TS 类型生成
```

#### 服务端新增（待明确 #A，仅在 aidea serve 内，不影响 proto 契约）
```
crates/core/src/llm.rs            + OllamaLlm / OpenAiLlm (impl Llm)
crates/probe/src/openai.rs        + OpenAiCompletionBackend (impl CompletionBackend)
crates/core/src/config.rs         + LlmBackend 枚举 + AIDEA_LLM_*/AIDEA_NES_* 字段
crates/core/src/agent.rs          + from_config/default_stack 选择 backend
```

---

### 6. 任务列表（按实现顺序，宏观 ≤5 分组；标注依赖）

> 约束：≤5 任务 / 首任务=基础设施 / 每组≥3 文件。ⓟ=依赖 proto 生成（随 ide-core），ⓢ=依赖 aidea serve 运行，ⓑ=依赖 sidecar 二进制（aidea/pg/ollama），ⓐ=依赖服务端新增 LLM backend 模块（待明确 #A）。

- **T01 项目基础设施与工程骨架**（依赖：无）
  - workspace 追加 `gui/src-tauri`；`gui/` 全套前端+配置文件；`src-tauri/Cargo.toml`、`build.rs`、`tauri.conf.json`（externalBin/资源）、`capabilities/default.json`、`main.rs` 空壳；前端 `main.tsx`/`App.tsx`/`styles`。
  - 交付：空白窗口可启动。

- **T02 后端栈自包含编排（sidecar + bootstrap）**（依赖：T01；ⓑ sidecar 二进制由 `fetch-binaries.sh` 提供）
  - `bootstrap/{mod,sidecar,pg,ollama,serve}.rs`、`model_backend.rs`、`config.rs`（keyring）、`resources/migrations/`、`resources/models/nes-tab/`、`resources/pg/`；`state.rs` 增加 bootstrap 句柄；`commands/{connection,model}.rs`（bootstrap_status/get/set_vendor_config/test_vendor）；前端 `BootstrapPage`+`SettingsPage`+`BootstrapProgress`。
  - 交付：一键拉起 PG→Ollama→模型→serve，进度/重试，模型后端切换。

- **T03 Rust 端 gRPC 桥接 + 领域逻辑**（依赖：T01；ⓟ 随 ide-core；ⓢ 需 serve 运行——由 T02 保障）
  - `grpc/{mod,client,stream}.rs`、`domain/{mod,core_config,quest,craft,collab}.rs`、`error.rs`、`commands/{chat,agent,quest,craft,collab,health}.rs`、`main.rs` 注册。
  - 交付：Tauri 命令契约稳定。

- **T04 前端业务页面**（依赖：T02、T03）
  - `pages/{ChatPage,QuestPage,CraftPage,CollabPage,HealthPage}.tsx`、`store/{chat,quest,craft,collab,health,model}.ts`、`api/invoke.ts`、`types/models.ts`、`components/{Layout,Sidebar,ErrorToast}.tsx`。
  - 交付：全部首版页面可用（本地/在线 chat、quest、craft、comment/lock/secret、健康概览）。

- **T05 macOS 打包（.app → .dmg，ad-hoc）**（依赖：T02+T03+T04；ⓑ 二进制打包）
  - `scripts/fetch-binaries.sh`、`scripts/build-dmg.sh`（拷贝 sidecar+权重、ad-hoc 签名、.dmg、预留 `NOTARIZE=1`）；完善 `tauri.conf.json` bundle。
  - 交付：本机可运行 `.app` + `.dmg`（分发受 Gatekeeper 限制，已注明）。

> **服务端追加任务（待明确 #A 拍板后并入）**：`T-server` 新增 LLM backend 模块（OllamaLlm+OpenAiLlm+OpenAI CompletionBackend+配置）——由 aidea 仓库单独 PR，GUI 侧 T02/T03 通过 env 驱动，无需等其合入即可先把 Mock 通路打通。

---

### 7. 共享知识（跨文件约定）

- **proto 生成产物**：随 `ide-core` 的 `tonic-build 0.11` 生成于 `target/.../out/ide_core.v1.rs`，编译进 `ide_core::v1`；GUI 复用（含 `agent_service_client`/`health_service_client`），**不重复 build**。
- **Command 命名**：`snake_case`、动词在前；流式事件 `{feature}-token/-done/-error`。
- **连接状态/错误一致性**：`GuiError{code,message}`；`ConnStatus = booting|connected|error`；gRPC 错误码透传为 `code`。
- **配置持久化**：非密钥（vendor kind、base_url、auto_bootstrap、tenant_id）→ `tauri-plugin-store`(JSON, AppData)；**密钥**（在线厂商 API Key、DB 密码、`AIDEA_ENC_KEY`）→ `keyring`(macOS Keychain)。前端/store **不含密钥明文**；`secret_get`/在线 Key 仅在内存瞬时使用。
- **sidecar 资源路径约定**：二进制与资源随 .app 置于 `Contents/Resources/`（macOS），Rust 用 `tauri::utils::platform::resource_dir()` 解析；`pg` 数据目录在 `AppData/<bundle>/pgdata`；Ollama 模型库在 `AppData/<bundle>/ollama`。
- **vendor → serve env 约定**（GUI 启动时注入，key 来自 keyring）：
  - `AIDEA_GRPC_ADDR=127.0.0.1:50051`
  - `AIDEA_DATABASE_URL=postgres://aidea:aidea@127.0.0.1:5432/aidea`（bundled PG）
  - local：`AIDEA_LLM_BACKEND=ollama` `AIDEA_NES_BACKEND=ollama` `AIDEA_LLM_MODEL=nes-tab:latest`
  - online：`AIDEA_LLM_BACKEND=openai` `AIDEA_NES_BACKEND=openai` `AIDEA_LLM_BASE_URL=<自定义>` `AIDEA_LLM_MODEL=<如 deepseek-chat>` `AIDEA_LLM_API_KEY=<keyring>`
- **模型名对齐**：端模型以 `nes-tab:latest` 暴露（Modelfile `FROM qwen2.5:0.5b`），对齐 `config.rs` 的 `model_name` 默认值，确保 NES 与 Chat/Quest 共用。
- **多轮上下文**：Chat 多轮由 serve 按 `session_id` 维护；前端复用同 `session_id` + 本地缓存；停止用 `CancellationToken`。
- **9090 解析**：`/healthz`→`ok\n`（非 JSON）；`/console`→文本+`--- json ---`内嵌 JSON；`/metrics`→Prometheus 文本（Rust 端解析，见 `crates/core/src/admin.rs`）。

---

### 8. 待明确事项（需用户/主理人确认）

> 前次「gRPC 缺口（quest/craft/comment/lock/secret 无 RPC）」已由「进程内复用 ide-core」解决，不再阻塞。本轮新增阻塞点如下。

- **#A【必须改服务端】LLM backend 模块缺失（最关键）**：aidea serve 当前 **只有 MockLlm（Chat/Quest）与 OllamaClient（NES）**，**无 OllamaLlm、无 OpenAiLlm、无 OpenAI CompletionBackend**。
  - **影响**：① 即便 bundle 了 Ollama+qwen2.5:0.5b，本地 chat/quest 仍走 MockLlm（假对话），无法兑现「端模型零配置对话」；② 在线厂商路由完全不存在。
  - **修改范围（bounded，不改 proto 契约）**：`crates/core/src/llm.rs`(+`OllamaLlm`/`OpenAiLlm`)、`crates/probe/src/openai.rs`(+`OpenAiCompletionBackend`)、`crates/core/src/config.rs`(+`LlmBackend` 枚举 + `AIDEA_LLM_*`/`AIDEA_NES_*`)、`crates/core/src/agent.rs`(`from_config`/`default_stack` 选择)。
  - **Key 传输**：GUI 以 env 在 serve 启动时注入 `AIDEA_LLM_API_KEY`（来自 keyring），**无需新增 gRPC RPC**。运行时切换=重启 serve（GUI 管理）。
  - **建议**：本设计按「T02/T03 先用 Mock 通路打通，#A 作为 aidea 仓库独立 PR 合入后由 env 驱动」推进。请拍板是否接受此服务端改动。

- **#B Craft 落盘**：`aidea serve` 用 `CliHost`（内存），即便 RunAgent 也不改真实文件；进程内 craft 复用 `CraftEngine` 同理。要让 craft 真正改盘需 GUI 进程内以真实 `FsHost` 跑 Core（ide-core 新增 `HostProvider`，不改 gRPC 服务端）。v1 默认仅展示 proposal/diff + `document_uri` 作「预期产物路径」，不落盘；是否增强为 (a) 待确认。

- **#C PG 初始化责任**：`aidea serve` **不建库、不跑迁移**。GUI bootstrap 必须：`initdb`(首次)→`pg_ctl start`→`createdb aidea`+建 role`aidea/aidea`→按序应用 `resources/migrations/*.sql`（用 `psql -f` 或 tokio-postgres 执行；需保证幂等/已应用标记）。请确认迁移 SQL 是否 `CREATE ... IF NOT EXISTS` 幂等（若否，bootstrap 需维护 applied 清单）。

- **#D 地址族**：serve 默认绑 `[::1]:50051`（IPv6），但 GUI 现以 `127.0.0.1:50051` 启动 serve（GUI 拥有生命周期，可强制 IPv4），前端连接同地址，**此问题已在自包含形态下消解**。保留为备注。

- **#E SSO**：默认 Noop；若服务端 `sso_enabled=true`，gRPC 需 Bearer。v1 不支持，待确认。

- **#F Quest 列表持久化**：`Quest::run` 单次返回，`aidea` 无历史存储；GUI「列表+详情」由前端本地运行历史承载，「耗时」=前端 `performance.now()` 包裹。可接受（不做服务端持久化）。

- **#G 公证扩展点**：ad-hoc 签名产 .dmg，分发受 Gatekeeper 拦截；`build-dmg.sh` 预留 `NOTARIZE=1`（需后续补 Apple 证书 + `notarytool` 流程）。仅文档/脚本占位。

- **#H 在线厂商连通测试位置**：建议 GUI 临时从 keyring 读 Key 直接探活 OpenAI 兼容端点（前端不持久化）；备选是新增 `AdminService.TestVendor` gRPC（需改 proto，不推荐）。默认采用 GUI 侧探活。

---

## Part B：任务依赖图（Mermaid）

```mermaid
graph TD
    T01[T01 项目基础设施与工程骨架] --> T02[T02 后端栈自包含编排 sidecar+bootstrap]
    T01 --> T03[T03 Rust 端 gRPC 桥接+领域逻辑]
    T02 --> T04[T04 前端业务页面]
    T03 --> T04
    T02 --> T05[T05 macOS 打包 .app/.dmg ad-hoc]
    T03 --> T05
    T04 --> T05
    Tserver[T-server LLM backend 模块 待明确#A] -.env驱动.-> T02
    Tserver -.env驱动.-> T03
```

> 说明：T02/T03 仅依赖 T01，可并行；T04 依赖 T02 的 bootstrap/模型契约与 T03 的 Command 契约；T05 依赖可运行产物。服务端 `T-server`（#A）独立推进，GUI 侧以 Mock 先打通、env 后驱动。

---

# 增量变更设计：Ollama → llama.cpp（`llama-server`）

> 作者：架构师 高见远（software-architect）
> 触发：用户已拍板走路径 **A**——把本地推理后端从 ollama 换成 **llama.cpp 的 `llama-server`**（sidecar 直连 GGUF），彻底消除 Intel x86_64 / macOS 13 上的 `ollama minos=14.0` 死结。
> 依据：对 `ide-m1` 全仓（Rust core/gui + CI + 打包脚本）的实地核查结论（见各节「代码核查结论」）。
> 形态不变：仍是单一 .dmg 自包含（GUI + llama-server + qwen2.5-0.5b.gguf + PostgreSQL + aidea serve），零配置本地对话 + 在线厂商切换。

---

## A. 系统设计方案（增量）

### A.1 实现方案 + 框架选型

#### A.1.1 核心思路（最小变更、结构化解死结）

- **不再依赖 ollama 封装层**。ollama 的 `/api/chat`、`/api/generate`、`/api/embeddings` 全部替换为 `llama-server` 的 **OpenAI 兼容端点**（`/v1/chat/completions`、`/v1/completions`、`/v1/embeddings`、`/health`）。
- **关键复用发现（代码核查）**：`ide-core` 里**早已存在** OpenAI 兼容实现——`crates/core/src/llm.rs` 的 `OpenAiLlm`（调 `/v1/chat/completions`）与 `crates/probe/src/openai.rs` 的 `OpenAiCompletionBackend`（同样调 `/v1/chat/completions`）。它们正是「在线厂商」用的后端。→ **本地 llama-server 路径直接复用这两个实现，仅把 base_url 指向 `http://127.0.0.1:<port>/v1`、api_key 留空**。这意味着：
  - 既有的 `OllamaLlm` / `OllamaClient` / `NesClient(OllamaClient)` 在「本地」路径上**不再被使用**（可保留代码但停用，或后续清理）；
  - **本变更对 aidea 对接层的真实改动量大幅低于直觉预期**（详见 A.6 / 关键问题③）。
- **权重直接加载**：`llama-server --model <gguf路径>` 直接读 GGUF，无需 `ollama create` / Modelfile / 联网拉取。随包内置的 `qwen2.5-0.5b.gguf` 现为「原生格式」，零重烘焙。

#### A.1.2 llama.cpp 版本策略（pin 固定 tag，自编译）

| 项 | 决策 | 理由 |
|----|------|------|
| 来源 | **上游官方 `ggml-org/llama.cpp`**，pin 一个 `bXXXX` release tag | 可复现、可审计；不跟 `master`（滚动、API 易漂） |
| 推荐 tag | **`b6782`**（2025-10，或更晚的稳定 tag；**实现 T1 时锁定具体值**，并写入 `build-dmg.yml` 常量） | 搜索可见官方 tag 已到 `b6782+`；任意近期 `bXXXX` 的 OpenAI 兼容 server 契约稳定 |
| 产物获取 | **osxcross + CMake 自编译**（与 aidea/aidea-gui 同源工具链），**不下载上游预编译 macos-x64 包** | 上游预编译包的 minos 不可控（正是 ollama 的坑）；自编译把 `MACOSX_DEPLOYMENT_TARGET=10.15` 写死，从根上消除死结 |
| 编译目标 | `x86_64-apple-darwin`（`-arch x86_64`），单二进制 `llama-server` | 用户机 Intel x86_64 硬约束；llama.cpp 默认把 ggml/llama 静态链进可执行文件，产出**单一自包含二进制**（无 `libggml*.dylib` 外置依赖） |
| BLAS | `GGML_BLAS=ON` + `GGML_BLAS_VENDOR=Apple`（Accelerate/vecLib） | Accelerate 自 macOS 10.3 起内置，**10.15 完全可用**，Intel CPU 推理显著加速；不引入额外 dylib |
| Metal | `GGML_METAL=OFF` | Metal 需 macOS 11+，且是 Apple Silicon 路径；Intel 机用不到，关掉避免引入 11+ 依赖 |
| OpenMP | 保持默认（llama.cpp 用自己的线程池，`GGML_OPENMP` 默认 off） | 避免链接 `libomp` 带来的 minos/依赖复杂度 |
| 仅编 server | `-DLLAMA_SERVER=ON -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TESTS=OFF` | 只产 `llama-server`，省 CI 时长与体积 |

> 推荐 CMake 调用（env 由 osxcross 环境提供 `x86_64-apple-darwinXX-clang` / SDK）：
> ```bash
> export MACOSX_DEPLOYMENT_TARGET=10.15
> cmake -S . -B build \
>   -DCMAKE_SYSTEM_NAME=Darwin \
>   -DCMAKE_OSX_SYSROOT="$OSXCROSS_SDK" \
>   -DCMAKE_OSX_ARCHITECTURES=x86_64 \
>   -DCMAKE_OSX_DEPLOYMENT_TARGET=10.15 \
>   -DGGML_METAL=OFF -DGGML_BLAS=ON -DGGML_BLAS_VENDOR=Apple \
>   -DLLAMA_SERVER=ON -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TESTS=OFF
> cmake --build build --target llama-server -j"$(nproc)"
> # 产物：build/bin/llama-server  →  拷贝为 .bundle/bin/llama-server-${TARGET_TRIPLE}
> ```

#### A.1.3 架构模式（不变部分沿用）

前端 React/Vite + Tauri v2 + gRPC(tonic) 直调本地 `aidea serve`；bootstrap 用 `std::process::Command` 拉起并监管 sidecar 的生命周期（PG / llama-server / serve）。**PostgreSQL sidecar、Tauri GUI、在线厂商切换能力全部原样保留。**

### A.2 文件列表（相对路径；改动 / 新增 / 删除）

| 文件 | 动作 | 改动要点 |
|------|------|----------|
| `.github/workflows/build-dmg.yml` | **改** | 删除「Download ollama macOS binary」整段（含 `OLLAMA_VERSION`/`patch-macos-minos.py` 调用/`lib/ollama` 拷贝/runner 三态探测）；**新增「Build llama.cpp (llama-server) via osxcross+CMake」step**（pin tag、env、拷贝单二进制）。更新 `verify` step 的 sidecar 列表与 minos 基线。 |
| `scripts/ci/patch-macos-minos.py` | **删** | 不再需要——llama.cpp 直接编成 10.15，无 minos 补丁步骤。 |
| `gui/scripts/fetch-binaries.sh` | **改** | `ollama` 拷贝段改为 `llama-server`（从 `LLAMA_SERVER_BIN` 指定路径拷贝，dev 期预编译产物）。 |
| `package-dmg.sh` | **改** | `SIDECARS=(... ollama ...)` → `... llama-server ...`；删除 `lib/ollama` runner 拷贝/`chmod` 逻辑（llama-server 单二进制无 lib 依赖）；`Resources/lib/` 注释去掉 ollama。 |
| `scripts/ci/verify-macho.sh` | **改** | `BUNDLE_MIN_MACOS` 由 `11.0` → `10.15`；sidecar 断言列表 `ollama` → `llama-server`；其余通用解析逻辑（`sidecar_check`/`payload_check`）基本复用。 |
| `scripts/ci/verify-payload.sh` | **改** | `ollama_check`（含 runner 三态 BUNDLED/ABSENT/EMBEDDED）整体替换为 `llama_server_check`：**单二进制存在性 + 体积下限（≈8 MiB）+ minos 观测**，删掉 runner 三态与对应单测形态。 |
| `gui/src-tauri/tauri.conf.json` | **改** | `externalBin`：`"bin/ollama"` → `"bin/llama-server"`；`macOS.minimumSystemVersion`：`"11.0"` → `"10.15"`。 |
| `gui/src-tauri/resources/models/nes-tab/Modelfile` | **删** | ollama 专属；llama-server 直接读 GGUF，不再需要。 |
| `scripts/nes-tab.Modelfile` | **删/确认** | 旧 `FROM qwen2.5:0.5b`（hub 引用）已无用，建议删除（先 grep 确认无引用）。 |
| `gui/src-tauri/src/bootstrap/ollama.rs` | **改名→`llamacpp.rs`** | 启动 `llama-server --model <gguf> --host 127.0.0.1 --port <LLAMACPP_PORT>`；轮询端口就绪；**删除 `ollama create nes-tab`**。 |
| `gui/src-tauri/src/bootstrap/mod.rs` | **改** | `pub mod ollama;` → `pub mod llamacpp;`；`bootstrap_stack` 里 `ollama::start` → `llamacpp::start`；注释里的「Ollama」措辞更新。 |
| `gui/src-tauri/src/state.rs` | **改** | `BootstrapHandles.ollama: Child` → `llamacpp: Child`（仅字段重命名）。 |
| `gui/src-tauri/src/model_backend.rs` | **改** | `VendorKind::Local` 注入改为：`AIDEA_LLM_BACKEND=local`、`AIDEA_NES_BACKEND=local`、`AIDEA_LLM_BASE_URL=http://127.0.0.1:<LLAMACPP_PORT>/v1`、`AIDEA_LLM_MODEL=<MODEL_ID>`。 |
| `crates/core/src/config.rs` | **改** | `LlmBackend`/`NesBackend` 枚举增加 `Local` 变体 + `parse`/`from_map` 处理（`"local"`→`Local`）；注释更新。 |
| `crates/core/src/agent.rs` | **改** | `build_llm` / `default_nes_backend` 增加 `Local` 分支：构造 `OpenAiLlm::new(base_url, "", model)` 与 `NesClient::with_backend(OpenAiCompletionBackend::new(...))`（base_url=localhost，无 key）。 |
| `crates/probe/src/ollama.rs` | **改（停用本地路径）** | `OllamaClient`/`NesClient` 不再被 `Local` 选用；保留 `MockOllamaClient`/`RuleBasedBackend`/cache/degradation/batch（仍被 Mock 与测试用）。建议加 `#[deprecated]` 标记 `OllamaClient`/`OllamaLlm`。 |
| `crates/core/src/llm.rs` | **改（小）** | `OllamaLlm` 保留但 `Local` 不再走它；可在文档/`#[deprecated]` 标注。无需新写 chat 逻辑（复用 `OpenAiLlm`）。 |
| `crates/probe/src/openai.rs` | **复用（保持不变）** | `OpenAiCompletionBackend` 直接作为本地 NES 后端。如需 embeddings，新增 `OpenAiEmbeddingClient`（见待明确）。 |
| `docs/sequence-diagram.mermaid` `docs/class-diagram.mermaid` | **改** | 已更新（见仓库）。 |
| `docs/system_design.md` | **改** | 本增量章节。 |

### A.3 数据结构和接口

#### A.3.1 `llama-server` 的 OpenAI 兼容端点契约（本地推理后端）

```
GET  /health                                  → { "status": "ok" }            # 就绪探测
POST /v1/chat/completions                     → { choices:[{ message:{role,content}, finish_reason }], usage }
     body: { model, messages:[{role,content}], stream:false, temperature }
POST /v1/completions                          → { choices:[{ text, finish_reason }] }   # 非 chat 补全（NES 可选走）
POST /v1/embeddings                           → { data:[{ embedding:[f32], index }] }   # 向量化（可选）
GET  /v1/models                               → { data:[{ id:"<model-id>" }] }
```
- **端口**：`LLAMACPP_PORT`（**推荐 `8080`**，llama.cpp 默认；与 ollama 的 11434 解耦，避免历史混淆）。base_url = `http://127.0.0.1:8080/v1`。
- **model 字段**：单模型常驻时，llama-server 接受任意 `model` 值；为稳定建议固定 `MODEL_ID`（如 `nes-tab` 或 `qwen2.5-0.5b`），并在启动时可加 `--alias nes-tab`（若所 pin tag 支持；否则忽略，服务端按单模型匹配）。
- **启动参数建议**：`llama-server --model "$GGUF" --host 127.0.0.1 --port 8080 --alias nes-tab`（不挂 GPU/Metal；CPU+Accelerate）。GGUF 路径 = `resource_dir()/resources/models/nes-tab/qwen2.5-0.5b.gguf`。

#### A.3.2 aidea → llama-server 调用关系（类图见 `class-diagram.mermaid`）

```
前端 ──gRPC──> aidea serve ──HTTP /v1/chat/completions──> llama-server(:8080) ──读──> qwen2.5-0.5b.gguf
                          └─ NES: OpenAiCompletionBackend ──/v1/chat/completions──> llama-server
```
- `LlmBackend::Local` ⇒ `OpenAiLlm`（已存在）指向 localhost；
- `NesBackend::Local` ⇒ `NesClient::with_backend(OpenAiCompletionBackend)`（已存在）指向 localhost；
- `OllamaLlm` / `OllamaClient` 在 `Local` 路径下**不再被引用**（仅 `Mock`/历史保留）。

### A.4 程序调用流程（时序图见 `sequence-diagram.mermaid`）

6 条主线与旧设计一致，**仅 ①、② 变更**：
1. **① 启动拉起**：PG → **llama-server（直接加载 GGUF，不再 `ollama create`）** → serve（env 注入 `BASE_URL=http://127.0.0.1:8080/v1`，`BACKEND=local`）。
2. **② Chat（本地）**：serve → llama-server `POST /v1/chat/completions`（替代旧 `ollama /api/chat`）。
3. ③ Chat（在线）、④ 模型后端切换（**仅重启 serve，llama-server 常驻不重启**）、⑤ Quest/Comment、⑥ 健康概览 —— 均不变。

### A.5 六关键问题回答（代码核查结论）

1. **llama.cpp 交叉编译可行性**：✅ 顺畅。osxcross+CMake 与既有 aidea 编译同源；build deps 仅 `cmake(≥3.17)` + osxcross clang + git（拉 tag）。产出**单二进制** `llama-server`（ggml/llama 默认静态链入），无外置 `libggml*.dylib`。推荐 env 见 A.1.2。CMake 调用与 env 见 A.1.2 代码块。
2. **Intel CPU + macOS 10.15（无 Metal）**：✅ 正常 CPU 推理。GGML CPU backend 不依赖 Metal；`GGML_BLAS=ON`+Apple(Accelerate) 在 10.15 可用（vecLib 自 10.3 内置）。无需关额外 feature（仅 `GGML_METAL=OFF`）。**结论：10.15 + Intel + 纯 CPU（Accelerate 加速）完全可行。**
3. **aidea 对接层改造成本（最大工作量评估）**：**远低于直觉**。代码核查证实 `OpenAiLlm`（`/v1/chat/completions`）与 `OpenAiCompletionBackend`（`/v1/chat/completions`）**已经存在且被「在线厂商」使用**。本地 llama-server 只是把它们指向 localhost。真实改动 = ① 枚举加 `Local` 变体（config.rs，~10 行）；② 工厂两处加 `Local` 分支（agent.rs，~15 行，复用既有 OpenAi 实现）；③ GUI vendor 注入改 env（model_backend.rs，~8 行）；④ bootstrap 从「启 ollama+create」改为「启 llama-server」（ollama.rs→llamacpp.rs，~60 行）。**无需新写任何 HTTP 请求/解析代码**。端点契约差异（ollama `/api/chat` vs llama.cpp `/v1/chat/completions`）= 已通过既有 `OpenAiLlm` 解决，无需适配层。
4. **sidecar 布局变更**：`bin/ollama-${TRIPLE}` + `lib/ollama/runners` → **`bin/llama-server-${TRIPLE}`（单二进制）**。tauri.conf `externalBin`、`package-dmg.sh` 的 `SIDECARS`、`fetch-binaries.sh`、verify 脚本断言同步改。**`patch-macos-minos.py` 步骤直接删除**（llama.cpp 编成 10.15，无需补丁）。
5. **模型权重**：✅ 确认。llama-server 直接读 GGUF（`--model`），无需 `ollama run`/`create`，权重零重烘焙。启动参数见 A.3.1。权重路径：`resources/models/nes-tab/qwen2.5-0.5b.gguf`（CI 已烘焙进包，build-dmg.yml 的 GGUF 下载 step 保留）。
6. **风险点**：
   - **CI 编译时长/体积**：llama.cpp 全量编译较重（免费 Linux runner + osxcross 约 10–20 min）。缓解：pin tag（tag 不变则不重编）、只编 `llama-server` target、关 examples/tests、CI 缓存 build 目录。
   - **llama.cpp API 漂移**：pin `bXXXX` tag + CI「minos=10.15 + 端口探活 `/health`」断言兜底。
   - **对话流式协议差异**：当前 `OpenAiLlm`/`OpenAiCompletionBackend` 都用 `stream:false`；与旧 ollama `stream:false` 一致，**本次零改动**。若未来要 SSE 流式，llama-server 支持 `/v1/chat/completions` 的 `stream:true`（SSE），属后续增强，不在 M1。
   - **FIM 补全语义**：`OpenAiCompletionBackend` 走 chat（非真 FIM `/v1/completions` 的 `input_prefix/suffix`），与既有「在线 NES」行为一致，M1 可接受；真 FIM 可后续用 llama.cpp 原生 `/completion` 增强。
   - **embeddings**：`NesClient::embed` 旧走 ollama `/api/embeddings`；本地走 llama-server `/v1/embeddings`（Qwen2.5-Instruct 在 llama.cpp 支持 embedding）。若所 pin tag/构建不支持或效果弱，应**优雅降级**（见待明确）。
   - **Tauri sidecar 权限**：llama-server 用 `std::process::Command` 启动（与 ollama 同源机制），loopback 监听无需网络权限弹窗；`hardened-runtime:false` + 未签名 sidecar 与 ollama 现状一致，可正常工作。
   - **minimumSystemVersion 下调到 10.15 的安全性**：用户机是 macOS 13，下调声明更宽松、无害；但需确认 Tauri 2 运行时自身 minos（历史为 10.13+），构建期 verify 会兜底。

### A.6 明确建议（架构师拍板）

1. **认可路径 A**：✅ 完全认可。从 ollama 封装层切换到 `llama-server` sidecar 是正确且结构性的解法，直接消除 minos 死结、复用既有 GGUF、且对接层改动极小。
2. **llama.cpp 版本**：✅ **pin 一个固定的 `bXXXX` tag（推荐 `b6782` 或实现时锁定更新的稳定 tag）**，用 osxcross+CMake 自编译（`MACOSX_DEPLOYMENT_TARGET=10.15`、`GGML_METAL=OFF`、`GGML_BLAS=ON`+Apple/Accelerate）。**不要**下载上游预编译 macos-x64 包（minos 不可控）。
3. **aidea 对接层最省事改法**：✅ **不新写任何推理 HTTP 代码**——`Local` 后端直接复用既有 `OpenAiLlm` 与 `OpenAiCompletionBackend`，仅把 `base_url` 指向 `http://127.0.0.1:8080/v1`、api_key 留空。枚举加 `Local` 变体 + 工厂两处分支 + GUI vendor 注入 + bootstrap 改写，即完成。这是本变更工作量最低、风险最小的路径。
4. **端口约定**：✅ `LLAMACPP_PORT=8080`（llama.cpp 默认，与 11434 解耦）。
5. **minos 基线**：✅ 全栈统一 **10.15**（aidea / llama-server 均 osxcross 编 10.15；tauri.conf `minimumSystemVersion=10.15`；verify 脚本 `BUNDLE_MIN_MACOS=10.15`）。

---

## B. 任务分解（实现顺序；标注依赖与改动文件）

> 约束对齐 SOP：≤5 主任务 / 首任务为基础设施类 / 每组≥3 文件。文档产出（本增量章节 + 两张 mermaid）为架构师交付物，不单列任务；但作为 T1–T5 的验收参考。

- **T1 · CI：用 osxcross+CMake 交叉编译 `llama-server` 替换 ollama 下载**（依赖：无）
  - 文件：`.github/workflows/build-dmg.yml`（删 ollama 下载/patch/runner 探测整段；新增 llama.cpp 编译 step：pin tag、env、`-DLLAMA_SERVER=ON`、拷贝 `bin/llama-server-${TARGET_TRIPLE}`、更新 verify 的 sidecar 列表与 minos 基线）、`gui/scripts/fetch-binaries.sh`（ollama→llama-server）、**删除** `scripts/ci/patch-macos-minos.py`。
  - 交付：CI 产出 x86_64、minos=10.15 的单二进制 `llama-server`，verify 通过。

- **T2 · 打包与校验脚本适配**（依赖：T1——约定 sidecar 名 `llama-server`）
  - 文件：`package-dmg.sh`（`SIDECARS` 的 `ollama`→`llama-server`；删 `lib/ollama` runner 拷贝/`chmod` 逻辑）、`scripts/ci/verify-macho.sh`（`BUNDLE_MIN_MACOS` 11.0→10.15；sidecar 列表 ollama→llama-server）、`scripts/ci/verify-payload.sh`（`ollama_check`+runner 三态 → `llama_server_check` 单二进制断言 + 删对应单测形态）。
  - 交付：.app 内 `Contents/Resources/bin/llama-server` 就位、macho 断言 minos=10.15、payload 断言通过。

- **T3 · Tauri 声明与端侧资源**（依赖：无）
  - 文件：`gui/src-tauri/tauri.conf.json`（`externalBin` `bin/ollama`→`bin/llama-server`；`minimumSystemVersion` 11.0→10.15）、**删除** `gui/src-tauri/resources/models/nes-tab/Modelfile` 与（确认无引用后）`scripts/nes-tab.Modelfile`。
  - 交付：Tauri 打包声明与 10.15 基线对齐，无 ollama 残留资源。

- **T4 · aidea 推理对接层（core crate）**（依赖：无——仅定义枚举/工厂与 env 约定，供 T5 引用）
  - 文件：`crates/core/src/config.rs`（`LlmBackend`/`NesBackend` 加 `Local` + parse/from_map）、`crates/core/src/agent.rs`（`build_llm`/`default_nes_backend` 加 `Local` 分支，复用 `OpenAiLlm`/`OpenAiCompletionBackend` + `NesClient::with_backend`）、`crates/core/src/llm.rs`（标注 `OllamaLlm` deprecated，Local 不走）、`crates/probe/src/ollama.rs`（标注 `OllamaClient` deprecated，Local 不走）、`crates/probe/src/openai.rs`（复用，可选补 `OpenAiEmbeddingClient`）。
  - 交付：env `AIDEA_LLM_BACKEND=local` / `AIDEA_NES_BACKEND=local` + `AIDEA_LLM_BASE_URL=http://127.0.0.1:8080/v1` 可被正确解析并路由到 llama-server。

- **T5 · GUI sidecar 编排 + vendor 注入**（依赖：T3 的 externalBin 名、T4 的 env/枚举约定）
  - 文件：`gui/src-tauri/src/bootstrap/ollama.rs`→**改名 `llamacpp.rs`**（启 `llama-server --model <gguf> --host 127.0.0.1 --port 8080`、轮询端口、删 `ollama create`）、`gui/src-tauri/src/bootstrap/mod.rs`（`pub mod ollama`→`llamacpp`；`ollama::start`→`llamacpp::start`）、`gui/src-tauri/src/state.rs`（`BootstrapHandles.ollama`→`llamacpp`）、`gui/src-tauri/src/model_backend.rs`（`VendorKind::Local` 注入 local + base_url + model env）。
  - 交付：一键拉起 PG→llama-server→serve，本地 chat/补全走 llama-server，在线切换仅重启 serve。

> 依赖图：T1 独立；T2 依赖 T1 的命名约定；T3 独立；T4 独立（产出约定）；T5 依赖 T3+T4。T1/T3/T4 可并行；T2 在 T1 后；T5 最后。

### B.1 依赖包 / 构建项列表

- **llama.cpp build deps（CI 新增）**：`cmake ≥ 3.17`（建议 3.27+）、osxcross 工具链（`x86_64-apple-darwinXX-clang` + SDK，已用于 aidea）、`git`（拉 tag）、`make`/`ninja`。**无新第三方 Rust 依赖**（aidea 侧仅改枚举/工厂，不引 crate）。
- **CI steps 新增项**：`Checkout llama.cpp @ <pinned bXXXX tag>` → `CMake configure`（见 A.1.2 env）→ `CMake build --target llama-server` → `拷贝 build/bin/llama-server → .bundle/bin/llama-server-${TARGET_TRIPLE}`。`scripts/ci/patch-macos-minos.py` 调用**移除**。

### B.2 共享知识（跨文件约定）

- **sidecar 命名规则**：所有 bundled 二进制落 `Contents/Resources/bin/<name>`，Tauri `externalBin` 追加 triple 后缀；本变更后端侧推理 sidecar = **`llama-server`**（单二进制，无 `lib/` 子目录）。
- **minos 基线（全栈统一）**：**10.15**。aidea 与 llama-server 均由 osxcross 以 `MACOSX_DEPLOYMENT_TARGET=10.15` 编译；tauri.conf `minimumSystemVersion=10.15`；verify 脚本 `BUNDLE_MIN_MACOS=10.15`。→ 不再有「minos 补丁」环节。
- **vendor → serve env 约定（Local 模式，GUI 注入）**：
  - `AIDEA_GRPC_ADDR=127.0.0.1:50051`
  - `AIDEA_DATABASE_URL=postgres://aidea:aidea@127.0.0.1:5432/aidea`
  - local：`AIDEA_LLM_BACKEND=local` `AIDEA_NES_BACKEND=local` `AIDEA_LLM_BASE_URL=http://127.0.0.1:8080/v1` `AIDEA_LLM_MODEL=<MODEL_ID>`（如 `nes-tab`）
  - online（不变）：`AIDEA_LLM_BACKEND=openai` `AIDEA_NES_BACKEND=openai` `AIDEA_LLM_BASE_URL=<自定义>` `AIDEA_LLM_MODEL=<如 deepseek-chat>` `AIDEA_LLM_API_KEY=<keyring>`
- **llama-server 启动约定**：`llama-server --model <gguf> --host 127.0.0.1 --port 8080 [--alias nes-tab]`；GGUF 路径 = `resource_dir()/resources/models/nes-tab/qwen2.5-0.5b.gguf`；常驻（不随 local/online 切换重启）。
- **端口**：llama-server = `8080`；aidea serve gRPC = `50051`；admin = `9090`（不变）。
- **退路（tolerate）**：llama-server 启动失败 → bootstrap 容忍（与旧 ollama 一致），serve 仍可起，本地 chat 降级为不可用/走在线；NES 仍走 `RuleBasedBackend` 兜底。

### B.3 待明确事项

- **#L1 embeddings 本地路径**：`NesClient::embed` 旧依赖 ollama `/api/embeddings`。本地走 llama-server `/v1/embeddings`（需 Qwen2.5-Instruct 在 llama.cpp 开启 embedding 支持）。若所 pin tag 不支持或效果弱，建议：(a) 新增 `OpenAiEmbeddingClient` 复用 `/v1/embeddings` 契约；或 (b) `embed` 在本地优雅降级（返回空/走规则），不影响 chat/补全主路径。请确认 NES 的 `embed` 在 M1 是否为关键路径（疑似仅用于向量化/检索，非核心对话）。
- **#L2 `--alias` 支持**：部分 llama.cpp tag 的 `llama-server` 未提供 `--alias`；若缺失，`model` 字段用任意固定 id（单模型常驻时服务端按单模型匹配，忽略 id）。建议以所 pin tag 实测为准，CI 用 `/v1/models` 或 `/health` 探活即可。
- **#L3 `scripts/nes-tab.Modelfile` 引用**：确认仓库内无其他脚本引用该根 Modelfile（grep 结果仅 build-dmg.yml 引用 `resources/models/nes-tab/` 下的 GGUF 与 Modelfile 目录），删除前做一次全仓 grep 确认。
- **#L4 Tauri 运行时 minos**：下调 `minimumSystemVersion` 到 10.15 前，构建期 verify 已兜底；另需确认 Tauri 2 自身运行时最低 macOS（历史 10.13+），确保 10.15 声明不矛盾。
- **#L5 模型 id 与 `model_name` 默认值**：`CoreConfig.model_name` 默认仍为 `nes-tab:latest`（ollama 语义残留），Local 模式下建议改为 `<MODEL_ID>`（如 `nes-tab`）仅作为请求 `model` 字段，不再代表 ollama tag；属命名整洁项，不影响功能。
