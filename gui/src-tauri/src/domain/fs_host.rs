//! 真实文件系统 Host（`FsHost`）——实现 `HostProvider`，把 craft 的编辑真正落盘
//! 到磁盘（决策 #B）。
//!
//! `ide_core` 自带的 `CliHost` 是内存实现，确认后不改任何文件。GUI 进程内以
//! `FsHost` 跑 `CraftEngine`，`confirm` 时 `apply_edit` 把 `new_text` 写回
//! `document_uri` 指向的真实路径，从而兑现「craft 真实落盘」。

use async_trait::async_trait;
use ide_core::host::provider::{HostError, HostEvent, HostProvider, TextEdit, GhostText};
use std::path::Path;

/// 把编辑写到真实文件系统的 Host 实现。
pub struct FsHost;

impl FsHost {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FsHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HostProvider for FsHost {
    async fn apply_edit(&self, edit: TextEdit) -> Result<(), HostError> {
        let path = Path::new(&edit.document_uri);
        let current = std::fs::read_to_string(path).unwrap_or_default();

        // 当 old_text 为空时，视为「追加」；否则要求 old_text 存在，避免误改。
        if !edit.old_text.is_empty() && !current.contains(&edit.old_text) {
            return Err(HostError::EditRejected(format!(
                "old_text 在 {} 中未找到，拒绝应用编辑",
                edit.document_uri
            )));
        }

        let updated = if edit.old_text.is_empty() {
            format!("{}{}", current, edit.new_text)
        } else {
            current.replacen(&edit.old_text, &edit.new_text, 1)
        };

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, updated)
            .map_err(|e| HostError::EditRejected(format!("写入 {} 失败: {e}", edit.document_uri)))?;
        Ok(())
    }

    async fn read_document(&self, uri: &str) -> Result<String, HostError> {
        std::fs::read_to_string(uri).map_err(|_| HostError::DocumentNotFound(uri.to_string()))
    }

    async fn show_ghost_text(&self, _ghost: GhostText) -> Result<(), HostError> {
        // GUI 侧 ghost text 由前端渲染，这里无需处理。
        Ok(())
    }

    async fn request_approval(&self, _action: &str) -> Result<bool, HostError> {
        // 人类已在 GUI 中点过确认，直接进入应用。
        Ok(true)
    }

    fn emit_event(&self, _event: HostEvent) {
        // 事件可在此桥接到前端日志；当前忽略。
    }
}
