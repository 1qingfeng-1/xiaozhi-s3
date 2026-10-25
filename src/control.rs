//! 控制面命令分发 — WS / MQTT 共用
//!
//! 两条传输的音频数据面不同（WS 二进制帧 vs UDP/AES），但控制面语义
//! （listen start/stop、tts abort、goodbye）完全一致。这里只抽象
//! “发送一条文本控制消息”，具体组帧/发布由各传输实现。
//!
//! `Connect` 不在这里真正建连：调用方需要处理“已连接时补发 hello/Ready”
//! 的差异，因此只通过 [`CommandOutcome::want_connect`] 返回。

use crate::app::{Channels, SessionCommand};
use crate::session;

/// 控制面发送能力
///
/// 仅本 crate 内部使用 (embassy 单线程), 不要求 future `Send`。
#[allow(async_fn_in_trait)]
pub trait ControlLink {
    async fn send_control(&mut self, text: &str) -> Result<(), &'static str>;
}

/// 命令处理结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandOutcome {
    /// 收到 Connect 请求 (调用方决定建连或补发 hello/Ready)
    pub want_connect: bool,
    /// 控制消息发送失败 (调用方应关闭会话)
    pub send_failed: bool,
}

/// 排空命令通道并发送控制消息
///
/// `connected`: 未连接时忽略 listen/stop/abort/goodbye (与两条传输原行为一致)。
pub async fn run_commands<L: ControlLink>(
    channels: &Channels,
    connected: bool,
    link: &mut L,
) -> CommandOutcome {
    let mut out = CommandOutcome {
        want_connect: false,
        send_failed: false,
    };
    loop {
        match channels.command.try_receive() {
            Ok(SessionCommand::Connect) => out.want_connect = true,
            Ok(SessionCommand::SendListenStart(mode)) => {
                let msg = if mode == xiaozhi_core::dialog::ListenMode::Auto {
                    session::LISTEN_START_AUTO
                } else {
                    session::LISTEN_START_MANUAL
                };
                if connected && link.send_control(msg).await.is_err() {
                    out.send_failed = true;
                    break;
                }
            },
            Ok(SessionCommand::SendListenStop) => {
                if connected && link.send_control(session::LISTEN_STOP).await.is_err() {
                    out.send_failed = true;
                    break;
                }
            },
            Ok(SessionCommand::SendTtsAbort) => {
                if connected && link.send_control(session::TTS_ABORT).await.is_err() {
                    out.send_failed = true;
                    break;
                }
            },
            Ok(SessionCommand::CloseSession) => {
                // 服务端即使忽略 goodbye 也无害
                if connected {
                    let _ = link.send_control(session::GOODBYE).await;
                }
            },
            Err(_) => break,
        }
    }
    out
}
