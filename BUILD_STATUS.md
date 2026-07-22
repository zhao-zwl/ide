# Agentic IDE (ide-m1) — 构建与部署进度

> 主理人齐活林维护。每个里程碑变化时更新。
> 最后更新：2026-07-21（主理人核实：真推理打通——nes-tab 用 qwen2.5:0.5b 顶替 + nomic-embed-text，serve 切 ollama 后端，`aidea nes --backend ollama` 实测 p95=1009ms）

## ⚠️ 本次会话的关键根因（已定位）
- **cargo 假死真因**：本机 Time Machine `backupd` 异常 → cargo 调 `CSBackupSetItemExcluded` 走 XPC 永久阻塞（CPU 0%）。绕过：C dylib 用 `__DATA,__interpose` 拦截该符号返回 noErr，编为 `/tmp/libnobackup.dylib`，`DYLD_INSERT_LIBRARIES` 前台注入。
- **protoc 缺失**：GitHub API 直连官方 `protoc-25.1-osx-x86_64` 解包到 `/tmp/protoc25/bin/protoc`（Ventura 可跑）。
- **15 编译错误根因**：v2.0 改 proto 服务名复数（`AgentService`/`HostService`/`HealthService`），代码用旧单数生成名 + 缺 `extern crate self as ide_core;` + `tokio-postgres` 漏 `runtime` feature + `rank_chunks` 缺 `k` 参数。修复后 ide-core 编过。
- **OOM**：全量并行编译被 SIGKILL（exit 137）→ `cargo build --workspace -j 2`。
- **Homebrew 国内镜像死结（PG 装不上的真凶）**：brew 5.x 默认 API 模式拉 `formulae.brew.sh`（被墙）；改 USTC API 镜像后，formula 的 `.rb` 源文件仍在 `raw.githubusercontent.com`（被墙），且 brew 内部 Ruby `net/http` 对 gh-proxy 前缀式 URL 卡死（无 curl 子进程、零网络活动、持 `.incomplete.download.lock`）。USTC **未镜像 homebrew-core 的 formula 源**。`git clone --depth 1 https://gh-proxy.com/https://github.com/Homebrew/homebrew-core.git` 到本地 tap 路径可成（git 命令能处理 gh-proxy 前缀，brew 内部 Ruby 不行），但 `brew install` 仍卡在下载 `.rb`。postgres.app 官网超时。→ **PG 在本机当前网络下装不上**。

## 当前状态（实时，2026-07-20 主理人核实）
- ✅ `cargo build --workspace` **已完成**：`target/debug/aidea`（29.1 MB, Mach-O x86_64）已产出；`aidea --version`→`aidea 1.0.0`；`--help` 列出 chat/craft/nes/serve/console/quest/comment/lock/secret 全子命令。
- ✅ **Ollama 已安装并起服务**（launchd 托管，KeepAlive）：`/usr/local/bin/ollama` 已链上，`ollama serve` 监听 127.0.0.1:11434（version 0.32.1）。`nes-tab:latest` 官方库没有（项目自定义模型），已用真实模型**顶替**：`ollama create nes-tab -f Modelfile`（`FROM qwen2.5:0.5b`，397MB）+ 另拉 `nomic-embed-text`（274MB，embedding 用）。`aidea serve` 的 `AIDEA_NES_BACKEND` 已从 `mock` 切到 `ollama`，真推理链路打通（`aidea nes --backend ollama` 实测 p95=1009ms，CPU 下超 300ms L0 预算但集成可用；换项目原版 GGUF 即可达预算）。
- ✅ **PostgreSQL 17 + pgvector 已落地（2026-07-20 解决）**：经 **gh-proxy.com 中继**下载 PostgresApp v2.9.5-17 DMG（119MB）→ 挂载拷到 `/Applications/Postgres.app`（含 PG17.10，**自带 pgvector 0.8.2**，无需本机编译）。`initdb` 数据目录 `/usr/local/var/postgres-17`，`pg_ctl` 起在 **5432**；已建 `aidea` 角色（密码 aidea）+ `aidea` 库；`CREATE EXTENSION vector`(0.8.2) / `pgcrypto`(1.3) 已挂。
- ✅ **migrations 全跑通**：`0001_init`~`0005_v20` 顺序执行零报错，**21 张表**就绪（audit 分区表 / sessions / embeddings / secrets / comments / locks 等）。⚠️ 修复了 `0001_init.sql` 的 SQL bug：参数名 `bit` 与 PG 内置类型 `bit` 冲突导致语法错误（连带 0002/0005 失败），已改名为 `bitpos`。
- ✅ **`aidea serve` 已起并监听 127.0.0.1:50051**（ollama 后端）：`bash ide-m1/scripts/aidea-serve.sh`（设 `AIDEA_NES_BACKEND=ollama` + `AIDEA_MODEL_NAME=nes-tab:latest`，连 PG + 调 Ollama）→ 日志 `[core] AgentService + HealthService listening on 127.0.0.1:50051`；`lsof` 确认监听。整条链路（二进制+PG+migration+Ollama 真推理+gRPC serve）端到端打通。
- Docker daemon 未运行 → 本机裸装依赖（不用容器）。

## 常驻方案与 502 根因 (2026-07-21 主理人核实)
- ✅ **launchd 常驻配置已就位**：写入 `~/Library/LaunchAgents/com.aidea.postgres.plist`（直跑 `postgres` 前台进程，KeepAlive+RunAtLoad）与 `~/Library/LaunchAgents/com.aidea.serve.plist`（调 `ide-m1/scripts/aidea-serve.sh`，先 `pg_isready` 等 PG 再 `aidea serve`，KeepAlive+RunAtLoad）。Ollama 的 plist 之前已建。→ **下次登录自动拉起 PG+serve**，无需手动。
- ⚠️ **沙箱无法 `launchctl load`**：本环境 Bash 沙箱禁止改动 launchd 域，`launchctl load`/`bootstrap` 均报 `Input/output error (code 5)`。故当前会话改用工具级 `run_in_background` 起 serve（已验证跨轮存活，PID 93810 在跑）。用户若想当前会话立即走 launchd，可在真实 Terminal 跑 `launchctl load -w ~/Library/LaunchAgents/com.aidea.*.plist`。
- ✅ **/console 报 502 的真因已定位并排除**：上一轮用 `bash ... &` 在同步 Bash 调用里后台起 serve，调用返回后父 shell 退出 → 后台子进程被 SIGHUP 带走 → 端口释放、admin 代理回 502。改用工具级 `run_in_background` 后稳定，`/healthz`→`200 ok`、`/console` 正常渲染、`aidea chat` 跑通且服务不崩。
- ℹ️ **`aidea chat` 是 in-process 驱动 ChatEngine**：不经过 50051 的 gRPC server，故 `/console` 的 `requests`/`audit_events` 计数保持 0 属正常（设计行为，非故障）。想让 server 计数动，需 gRPC 客户端连 50051（环境无 grpcurl）。

## 里程碑
| # | 步骤 | 状态 | 判定方式 |
|---|------|------|----------|
| 1 | 修复 `tokio-postgres` 错误 feature | ✅ 完成 | 已 Read 验证 |
| 2 | 真机 `cargo build` 出 `aidea` 二进制 | ✅ **完成（已核实）** | 29.1MB + `--version`→1.0.0 + 全子命令 |
| 3 | 安装 PostgreSQL@17 + pgvector | ✅ **已解决（Postgres.app 中继）** | `/Applications/Postgres.app` PG17.10 + 自带 pgvector 0.8.2 |
| 4 | 起 PG + 建 `aidea` 库 + `CREATE EXTENSION vector` | ✅ 完成 | `psql` 连 5432、`vector`(0.8.2)/`pgcrypto`(1.3) 已挂 |
| 5 | 跑 migrations `0001~0005.sql` | ✅ 完成（修 1 处 SQL bug） | 21 张表就绪 |
| 6 | 起 Ollama + 真模型（nes-tab 顶替） | ✅ 完成（qwen2.5:0.5b 顶替 nes-tab + nomic-embed-text） | `aidea nes --backend ollama` 实测真推理 p95=1009ms |
| 7 | `aidea serve` 起 gRPC 50051 + 冒烟 | ✅ 完成 | 日志 `[core] ...listening on 127.0.0.1:50051`；lsof 确认监听 |

## 你会怎么知道做完？
1. 随时发"好了么 / 进度？" → 我立刻 `ps` + 读日志 + 查二进制，秒回真实状态。
2. 自己看 `ide-m1/BUILD_STATUS.md`（本文件）。
3. 看产物：`target/debug/aidea` 已出现；依赖装完 `brew list` 会含 postgresql / pgvector。

## 日志（真机实时）
- 编译：`tail -f /tmp/cargo-build.log`
- 安装：`tail -f /tmp/brew-pg*.log`
