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
