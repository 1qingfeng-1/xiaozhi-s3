//! 小智音箱 — ESP32-S3 智能音箱应用
//!
//! 模块结构:
//! - audio: opus 编解码 + 声音唤醒
//! - bsp:   板级支持包 (编译时选板)
//! - dialog 入口在 xiaozhi-core (纯逻辑状态机)
//! - wifi:  WiFi 连接管理 + 配网
//! - session: WebSocket 会话传输层 (xiaozhi 协议)
//! - app:   主应用编排 (状态机驱动的音频/网络/灯效)

#![no_std]
extern crate alloc;

// 子模块声明
pub mod app;
pub mod audio;
pub mod bsp;
pub mod macros;
pub mod mqtt_session;
pub mod ota_check;
pub mod session;
pub mod wifi;
