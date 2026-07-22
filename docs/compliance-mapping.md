# 等保三级（代码级加固）控制项 ↔ 实现落点映射

> 适用范围：智能体 IDE v2.0 Stage A（T19 协作 / T20 等保三级）。
> 本文件是 **代码级加固** 的落点说明，**不是真实的等保三级测评认证**。
> 真实测评必须由具备资质的测评机构（authorized assessor）依据
> GB/T 22239-2019《信息安全技术 网络安全等级保护基本要求》对**完整部署形态**
> （含网络、主机、应用、运维、管理制度）开展，本仓库仅覆盖其中的「应用 / 数据」部分控制点。

## 0. 总览

| 等保三级控制类 | 控制项编号 | 本实现落点 | 完成度 |
| --- | --- | --- | --- |
| 安全计算环境 | 8.1.2 访问控制 | RLS 行级隔离（`0005_v20.sql`）+ 六位权限掩码（`permissions`/`principal`） | 代码级 ✅ |
| 安全计算环境 | 8.1.3 安全审计 | `audit` 追加-only + `prev_hash`/`row_hash` 防篡改链（`audit_verify_chain()`） | 代码级 ✅ |
| 安全计算环境 | 8.1.4 数据完整性 | 审计哈希链 + `pgcrypto` 校验；所有 INSERT 经 RLS `WITH CHECK` | 代码级 ✅ |
| 安全计算环境 | 8.1.5 数据保密性 | `pgcrypto` 静态加密 `secrets`（`pgp_sym_encrypt`/`pgp_sym_decrypt`） | 代码级 ✅ |
| 安全通信网络 | 8.1.6 通信加密 | `tokio-postgres::NoTls` 仅用于私有可信网络 / unix socket；DEK 不落库 | 代码级 ⚠️（需 sslmode/TLS 终结束） |
| 安全计算环境 | 8.1.7 剩余信息保护 | `secrets` 表不存明文；`audit.detail_encrypted` 可加密镜像列已预留 | 代码级 ✅ |

> 标注 ⚠️ 的项属于「部署形态」依赖（TLS 终止、KMS 托管密钥、网络隔离），
> 本阶段提供代码侧的准备（密钥不落库、私有网络假设），但**真实达标需运维层补齐**。

---

## 1. 8.1.2 访问控制（Access Control）

### 控制要求（要点）
- 应对登录的用户分配账户和权限。
- 应重命名或删除默认账户，修改默认账户的口令。
- 应及时删除或停用多余的、过期的账户。
- 应授予管理用户所需的最小权限。

### 本实现落点
1. **租户级行级隔离（多租户就绪）**：`migrations/0005_v20.sql`
   对 `audit` / `context_sources` / `ckg_symbols` / `ckg_edges` / `comments` /
   `locks` / `secrets` 共 7 张表新增 `tenant_id text NOT NULL DEFAULT 'default'`
   列并 `ENABLE ROW LEVEL SECURITY`，策略为：
   ```sql
   USING (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
   WITH CHECK (tenant_id = coalesce(current_setting('app.tenant_id', true), tenant_id))
   ```
   - `coalesce(current_setting('app.tenant_id', true), tenant_id)` 的设计：
     当连接**未**设置 `app.tenant_id`（开发 / 历史 SQL 工具 / 测试）时，
     行为等价于「不加隔离」，向后兼容；当 Rust 的 `Pg*` 存储连接后执行
     `SET app.tenant_id = $1`（见 `collab.rs` / `security.rs` / `audit.rs` /
     `ckg.rs`），则强制按租户隔离。
   - `audit` 表采用 **RESTRICTIVE** 策略，与 `0001` 的 permissive 追加-only
     策略以 AND 组合，避免 OR 放宽为可写。

2. **应用级权限掩码**：`crates/core/src/permissions.rs` 的六位权限
   （Read=1 / Generate=2 / Modify=4 / Execute=8 / Commit=16 / Audit=32）
   经 `principal::check_permission` 在 `with_governance` 链路中强制，
   与 SQL `perm_mask` 域（0..63）字节一致（见 `config.rs` 的 `clamp`）。

### 代码级完成度：✅
> 真实测评还需：账户生命周期管理（创建/过期/停用）、默认口令整改、管理最小权限的运维流程。

---

## 2. 8.1.3 安全审计（Security Audit）

### 控制要求（要点）
- 应启用安全审计功能，审计覆盖到每个用户。
- 审计记录应包括事件的日期和时间、用户、事件类型、事件是否成功等。
- 应对审计记录进行保护，定期备份，避免受到未预期的删除、修改或覆盖。

### 本实现落点
1. **追加-only 审计表**（`0001` 已建）：`audit` 分区表 + `BEFORE UPDATE/DELETE`
   触发器（各分区显式挂载，绕开分区不继承触发器限制）+ RLS 仅允许 INSERT。
2. **防篡改哈希链（T20 新增）**（`0005_v20.sql`）：
   - 新增 `prev_hash` / `row_hash` 列。
   - `audit_chain_payload(action, perm_bit, tenant_id, prev_hash)` 产出规范化字符串
     `action|perm_bit|tenant_id|prev_hash`（**刻意排除** `detail` / `ts` / `id`，
     避免 JSON 规范化与 timestamptz 文本跨边界不一致）。
   - `audit_row_hash(...)` = `sha256(payload)` 的十六进制串（pgcrypto）。
   - `audit_chain_before_insert()` BEFORE INSERT 触发器：若客户端已提供
     `prev_hash`/`row_hash`（Rust `PgAuditSink` 会提供，用于可审计证明），
     则保留；否则服务端权威填充。
   - `audit_verify_chain()` 返回任何 `row_hash` 与重算值不符或缺失的行，
     用于检测插入/删除/重排/篡改。
3. **Rust 侧镜像**：`crates/core/src/audit.rs` 的 `AuditEvent::row_hash` 与
   SQL `audit_row_hash` 使用**完全相同**的规范化格式，便于跨端校验；
   `PgAuditSink::record` 在插入前查询上一行 `row_hash` 作为 `prev_hash` 并写链。

### 代码级完成度：✅
> 真实测评还需：审计记录异地备份、审计存储容量与留存周期策略、审计管理员与业务管理员职责分离。

---

## 3. 8.1.4 数据完整性（Data Integrity）

### 控制要求（要点）
- 应采用校验技术保证重要数据在传输和存储过程中的完整性。
- 应采用密码技术保证重要数据在存储过程中的保密性（与 8.1.5 协同）。

### 本实现落点
1. **存储完整性**：审计哈希链（见 §2）使 `audit` 表任一行的篡改/缺失可被
   `audit_verify_chain()` 检出。
2. **写入完整性**：所有租户相关表的 RLS `WITH CHECK` 强制写入行的
   `tenant_id` 与当前会话一致，杜绝越权跨租户写入。
3. **重要数据保密性** 由 `pgcrypto` 静态加密提供（见 §4）。

### 代码级完成度：✅

---

## 4. 8.1.5 数据保密性（Data Confidentiality）

### 控制要求（要点）
- 应采用密码技术保证重要数据在存储过程中的保密性。

### 本实现落点
`migrations/0005_v20.sql` 定义 `secrets` 表 + 两个 `pgcrypto` 函数：
```sql
CREATE OR REPLACE FUNCTION set_secret(p_tenant, p_name, p_value, p_key)
  -- INSERT ... ON CONFLICT (tenant_id, name)
  --   DO UPDATE SET value_encrypted = pgp_sym_encrypt(p_value, p_key);

CREATE OR REPLACE FUNCTION get_secret(p_tenant, p_name, p_key)
  -- SELECT pgp_sym_decrypt(value_encrypted, p_key) FROM secrets ...;
```
- 密钥（DEK）来自 `AIDEA_ENC_KEY`，由 `CoreConfig::enc_key` 承载，
  **仅经私有可信连接下发到 Postgres，绝不落库**。
- Rust 侧 `crates/core/src/security.rs` 的 `PgSecretStore` 封装
  `set_secret` / `get_secret`，并在每个操作的事务内 `SET LOCAL app.tenant_id`。
- 通用辅助 `pg_encrypt_text` / `pg_decrypt_text` 可对任意敏感列（如
  `audit.detail_encrypted` 加密镜像列）加密。

### 代码级完成度：✅
> 真实测评还需：DEK 由 KMS 托管而非环境变量明文；密钥轮换流程；pgcrypto 之外考虑列级 / 表空间加密与传输 TLS。

---

## 5. 8.1.6 通信加密（Communication Encryption）

### 控制要求（要点）
- 应采用密码技术保证通信过程中数据的保密性。
- 应采用可信的网络连接方式。

### 本实现落点
- `tokio-postgres` 使用 `NoTls`，部署假设为**私有可信网络 / unix socket**
  （见 `crates/core/src/audit.rs`、`ckg.rs`、`collab.rs`、`security.rs` 注释）。
- DEK 仅在受信任连接内传输，不写库、不写日志。

### 代码级完成度：⚠️（部署形态依赖）
> 真实测评需补齐：`psql` 连接启用 `sslmode=verify-full`（或前置 TLS 终止 /
> mTLS）；管理平面与数据平面网络隔离；gRPC 启用 TLS（ProtoBus G6 冻结，
> 不在本阶段范围）。

---

## 6. 8.1.7 剩余信息保护（Residual Information Protection）

### 控制要求（要点）
- 应保证鉴别信息所在的存储空间被释放或重新分配前得到完全清除。

### 本实现落点
- `secrets` 表只存 `value_encrypted`（`bytea`），**不存明文**；读取时经
  `pgp_sym_decrypt` 在查询内解密，明文不在表内残留。
- `audit.detail_encrypted` 加密镜像列已预留（`0005`），可替代明文 `detail`
  承载高敏信息，避免审计列残留敏感明文。
- `CoreConfig::enc_key`（DEK）标注 `never logged`，仅在内存中经连接下发。

### 代码级完成度：✅

---

## 7. T19 协作（非等保项，平台化能力）

与等保三级无直接映射，但复用同一 `tenant_id` 概念，确保租户隔离语义一致：
- `crates/core/src/collab.rs`：`Comment`/`CommentStore`（`InMemoryCommentStore`
  / `PgCommentStore`）、`Lock`/`LockStore`（`InMemoryLockStore`）。
- 所有协作表纳入 RLS（§1），`PgCommentStore` 事务内 `SET LOCAL app.tenant_id`。
- CLI：`aidea comment <list|add|resolve>`、`aidea lock <acquire|release|show>`。

---

## 8. 结论与后续清单

### 代码级加固：✅ 已完成
本仓库已实现并自测（纯逻辑 `#[test]` / `#[tokio::test]`，无外部依赖）：
- RLS 多租户就绪（7 表 + 策略 SQL 静态自洽断言）。
- 审计追加-only + SHA-256 防篡改链 + `audit_verify_chain()`。
- `pgcrypto` 静态加密 `secrets` + `AIDEA_ENC_KEY` 不落库。
- 协作模块租户隔离。

### 真实等保三级测评认证：需另行委托 ❌
本阶段**不构成**任何合规认证结论。正式测评前至少需补齐（运维 / 部署层）：
1. TLS 终止 / `sslmode=verify-full` / gRPC TLS。
2. KMS 托管 `AIDEA_ENC_KEY` + 密钥轮换。
3. 账户生命周期（创建/过期/停用）、默认账户整改、管理最小权限流程。
4. 审计异地备份、留存周期、审计/业务管理员职责分离。
5. 多租户端到端渗透测试与网络隔离验证。
6. 管理制度、人员、建设、运维等「通用要求」类控制项文档化。
