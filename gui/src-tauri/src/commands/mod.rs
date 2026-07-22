//! Tauri 命令层（GUI ⇄ Rust 后端）。
//!
//! 命令按领域分组：`connection`（启动/模型后端配置）、`chat` / `agent`（流式
//! 交互）、`quest` / `craft`（进程内自治与编辑）、`collab`（comment/lock/secret）、
//! `health`（9090 概览）、`model`（本地模型列表/切换）。

pub mod agent;
pub mod chat;
pub mod collab;
pub mod connection;
pub mod craft;
pub mod health;
pub mod model;
pub mod quest;
