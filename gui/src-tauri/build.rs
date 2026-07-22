//! Tauri v2 构建钩子。
//!
//! 不重复 build proto —— gRPC 类型由 `ide-core` 的 `tonic-build` 生成并经由
//! `ide_core::v1` 复用。这里仅运行 Tauri 的代码生成（上下文、权限 schema 等）。

fn main() {
    tauri_build::build()
}
