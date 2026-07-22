//! 进程内协作能力：comment / lock / secret。
//!
//! comment 与 secret 直连 bundled PostgreSQL（`PgCommentStore` / `PgSecretStore`）；
//! lock 采用内存存储（与 CLI 同构，编辑存在性提示为会话级）。

use crate::domain::core_config::build_core_config;
use crate::domain::current_vendor;
use crate::error::{GuiError, GuiResult};
use crate::ipc::{
    CommentAddRequest, CommentDto, LockDto, SecretGetRequest, SecretSetRequest,
};
use ide_core::collab::{
    Comment, CommentStore, InMemoryCommentStore, InMemoryLockStore, Lock, LockStore,
    PgCommentStore,
};
use ide_core::security::PgSecretStore;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// 选取 comment 存储：优先 bundled PG，不可达时回退内存（不持久化）。
async fn comment_store(app: &AppHandle) -> Arc<dyn CommentStore> {
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    match PgCommentStore::connect(&cfg.database_url, &cfg.tenant_id).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!("comment pg 不可用 ({e})，回退内存存储");
            Arc::new(InMemoryCommentStore::new())
        }
    }
}

/// 新增一条代码评论。
pub async fn comment_add(app: &AppHandle, req: CommentAddRequest) -> GuiResult<CommentDto> {
    let store = comment_store(app).await;
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let author = cfg.user_id.clone();
    let comment = Comment::new(&cfg.tenant_id, &req.file, req.line, req.line, &author, &req.body);
    let id = store.add(&comment).await.map_err(GuiError::from)?;
    Ok(CommentDto {
        id,
        tenant_id: cfg.tenant_id,
        file: req.file,
        line_start: req.line,
        line_end: req.line,
        author,
        body: req.body,
        resolved: false,
        created_at: comment.created_at,
    })
}

/// 列出某文件的评论。
pub async fn comment_list(app: &AppHandle, file: String) -> GuiResult<Vec<CommentDto>> {
    let store = comment_store(app).await;
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let comments = store
        .list_for_file(&cfg.tenant_id, &file)
        .await
        .map_err(GuiError::from)?;
    Ok(comments.into_iter().map(CommentDto::from).collect())
}

/// 解决一条评论。
pub async fn comment_resolve(app: &AppHandle, id: String) -> GuiResult<bool> {
    let store = comment_store(app).await;
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let ok = store.resolve(&cfg.tenant_id, &id).await.map_err(GuiError::from)?;
    Ok(ok)
}

/// 内存锁存储（会话级编辑存在性提示）。进程级单例，保证跨调用共享同一把锁。
fn lock_store() -> Arc<dyn LockStore> {
    use std::sync::OnceLock;
    static STORE: OnceLock<Arc<InMemoryLockStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(InMemoryLockStore::new()))
        .clone()
}

/// 获取某文件的编辑锁。
pub async fn lock_acquire(app: &AppHandle, file: String) -> GuiResult<LockDto> {
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let store = lock_store();
    let owner = cfg.user_id.clone();
    let lock = Lock::new(&cfg.tenant_id, &file, &owner);
    store.acquire(&lock).await.map_err(GuiError::from)?;
    Ok(LockDto::from(lock))
}

/// 释放某文件的编辑锁。
pub async fn lock_release(app: &AppHandle, file: String) -> GuiResult<()> {
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    lock_store()
        .release(&cfg.tenant_id, &file)
        .await
        .map_err(GuiError::from)?;
    Ok(())
}

/// 查看某文件的编辑锁持有者。
pub async fn lock_show(app: &AppHandle, file: String) -> GuiResult<Option<LockDto>> {
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let lock = lock_store()
        .get(&cfg.tenant_id, &file)
        .await
        .map_err(GuiError::from)?;
    Ok(lock.map(LockDto::from))
}

/// 写入一条密钥（pgcrypto 加密落盘）。
pub async fn secret_set(app: &AppHandle, req: SecretSetRequest) -> GuiResult<()> {
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let store = PgSecretStore::connect(&cfg.database_url, &cfg.tenant_id)
        .await
        .map_err(GuiError::from)?;
    store
        .set(&cfg.tenant_id, &req.name, &req.value, &cfg.enc_key)
        .await
        .map_err(GuiError::from)?;
    Ok(())
}

/// 读取一条密钥（用 `AIDEA_ENC_KEY` 解密）。
pub async fn secret_get(app: &AppHandle, req: SecretGetRequest) -> GuiResult<Option<String>> {
    let vendor = current_vendor(app);
    let cfg = build_core_config(&vendor);
    let store = PgSecretStore::connect(&cfg.database_url, &cfg.tenant_id)
        .await
        .map_err(GuiError::from)?;
    let value = store
        .get(&cfg.tenant_id, &req.name, &cfg.enc_key)
        .await
        .map_err(GuiError::from)?;
    Ok(value)
}
