//! 协作命令：comment / lock / secret。

use crate::domain;
use crate::error::GuiResult;
use crate::ipc::{
    CommentAddRequest, CommentDto, LockDto, SecretGetRequest, SecretSetRequest,
};
use tauri::AppHandle;

/// 新增一条代码评论（优先 bundled PG，不可达回退内存）。
#[tauri::command]
pub async fn comment_add(app: AppHandle, req: CommentAddRequest) -> GuiResult<CommentDto> {
    domain::collab::comment_add(&app, req).await
}

/// 列出某文件的评论。
#[tauri::command]
pub async fn comment_list(app: AppHandle, file: String) -> GuiResult<Vec<CommentDto>> {
    domain::collab::comment_list(&app, file).await
}

/// 解决一条评论。
#[tauri::command]
pub async fn comment_resolve(app: AppHandle, id: String) -> GuiResult<bool> {
    domain::collab::comment_resolve(&app, id).await
}

/// 获取某文件的编辑锁。
#[tauri::command]
pub async fn lock_acquire(app: AppHandle, file: String) -> GuiResult<LockDto> {
    domain::collab::lock_acquire(&app, file).await
}

/// 释放某文件的编辑锁。
#[tauri::command]
pub async fn lock_release(app: AppHandle, file: String) -> GuiResult<()> {
    domain::collab::lock_release(&app, file).await
}

/// 查看某文件的编辑锁持有者。
#[tauri::command]
pub async fn lock_show(app: AppHandle, file: String) -> GuiResult<Option<LockDto>> {
    domain::collab::lock_show(&app, file).await
}

/// 写入一条密钥（pgcrypto 加密落盘）。
#[tauri::command]
pub async fn secret_set(app: AppHandle, req: SecretSetRequest) -> GuiResult<()> {
    domain::collab::secret_set(&app, req).await
}

/// 读取一条密钥（用 `AIDEA_ENC_KEY` 解密）。
#[tauri::command]
pub async fn secret_get(app: AppHandle, req: SecretGetRequest) -> GuiResult<Option<String>> {
    domain::collab::secret_get(&app, req).await
}
