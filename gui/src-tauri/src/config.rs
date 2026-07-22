//! GUI 侧配置持久化。
//!
//! 约定（来自设计文档「共享知识」）：
//!   * **非密钥**（vendor kind、base_url、auto_bootstrap、tenant_id）→
//!     `tauri-plugin-store`（JSON，AppData），可由 `load/save_vendor_config` 读写。
//!   * **密钥**（在线厂商 API Key、DB 密码、`AIDEA_ENC_KEY`）→ `keyring`
//!     （macOS Keychain）。前端/store 不持有密钥明文；仅内存瞬时使用。

use crate::error::{GuiError, GuiResult};
use crate::ipc::VendorConfig;
use serde_json::json;
use tauri::{AppHandle, Manager};
use tauri_plugin_store::StoreExt;

/// store 文件名。
const STORE_FILE: &str = "aidea.json";
/// keyring 服务名（macOS Keychain 条目前缀）。
const KEYRING_SERVICE: &str = "com.aidea.gui";

/// 从 store 读取模型后端配置；缺失时回退默认值。
pub fn load_vendor_config(app: &AppHandle) -> GuiResult<VendorConfig> {
    let store = app.store(STORE_FILE).map_err(|e| GuiError::vendor(e.to_string()))?;
    let cfg = store
        .get("vendor")
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or(VendorConfig {
            kind: crate::ipc::VendorKind::Local,
            base_url: None,
            local_model: "nes-tab:latest".to_string(),
            model: None,
        });
    Ok(cfg)
}

/// 将模型后端配置写入 store（不含任何密钥）。
pub fn save_vendor_config(app: &AppHandle, cfg: &VendorConfig) -> GuiResult<()> {
    let store = app.store(STORE_FILE).map_err(|e| GuiError::vendor(e.to_string()))?;
    store
        .set("vendor", json!(cfg))
        .map_err(|e| GuiError::vendor(e.to_string()))?;
    store.save().map_err(|e| GuiError::vendor(e.to_string()))?;
    Ok(())
}

/// 读取自动拉起开关（默认开启）。
pub fn load_auto_bootstrap(app: &AppHandle) -> bool {
    let store = match app.store(STORE_FILE) {
        Ok(s) => s,
        Err(_) => return true,
    };
    store
        .get("auto_bootstrap")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 写入自动拉起开关。
pub fn save_auto_bootstrap(app: &AppHandle, enabled: bool) -> GuiResult<()> {
    let store = app.store(STORE_FILE).map_err(|e| GuiError::vendor(e.to_string()))?;
    store
        .set("auto_bootstrap", json!(enabled))
        .map_err(|e| GuiError::vendor(e.to_string()))?;
    store.save().map_err(|e| GuiError::vendor(e.to_string()))?;
    Ok(())
}

/// 从 keyring 读取密钥；不存在返回 None。
pub fn get_secret(key: &str) -> GuiResult<Option<String>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key)
        .map_err(|e| GuiError::vendor(format!("keyring entry error: {e}")))?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        // 条目不存在视为「未配置」，不是错误。
        Err(keyring::Error::NoEntry) | Err(keyring::Error::NoStorageAccess) => Ok(None),
        Err(e) => Err(GuiError::vendor(format!("keyring read error: {e}"))),
    }
}

/// 将密钥写入 keyring。
pub fn set_secret(key: &str, value: &str) -> GuiResult<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key)
        .map_err(|e| GuiError::vendor(format!("keyring entry error: {e}")))?;
    entry
        .set_password(value)
        .map_err(|e| GuiError::vendor(format!("keyring write error: {e}")))?;
    Ok(())
}

/// 从 keyring 删除密钥。
pub fn delete_secret(key: &str) -> GuiResult<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key)
        .map_err(|e| GuiError::vendor(format!("keyring entry error: {e}")))?;
    // 删除失败（如条目不存在）不视为错误。
    let _ = entry.delete_credential();
    Ok(())
}
