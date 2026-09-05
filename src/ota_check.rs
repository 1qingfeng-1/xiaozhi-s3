//! OTA 检查 (网络层): GET :8003/xiaozhi/ota/ → 清单解析 → 版本比较
//!
//! 开机由 [`check_task`] 后台任务触发一次 (独立任务, 不阻塞会话/主循环启动;
//! 服务器 :8003 不可达时 connect 自行超时, 不影响其余逻辑)。结果经 defmt 打印
//! (已是最新 / 有新版本 / 检查失败)。
//! 下载/烧写需分区表改造 (双 app + otadata), 见 ROADMAP M9;
//! 本模块当前只做检查与上报 (与 C++ ota.cc 的检查阶段对齐)。

use embassy_net::{Ipv4Address, Stack, tcp::TcpSocket};

use defmt::{error, info, warn};

use xiaozhi_core::ota;

use crate::mk_static;

/// 检查结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// 服务器最新版本
    pub latest_version: heapless::String<32>,
    /// 是否比当前固件新
    pub update_available: bool,
}

/// 向服务器查询固件更新
pub async fn check(stack: Stack<'static>, server: Ipv4Address) -> Option<UpdateReport> {
    // check() 消费一次性 Stack<'static> (调用方 mk_static!), 故 rx/tx 亦用
    // mk_static! 常驻 (替代原 vec!().leak() 每次泄漏 1536B)
    let rx = mk_static!([u8; 1024], [0u8; 1024]);
    let tx = mk_static!([u8; 512], [0u8; 512]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.connect((server, 8003)).await.ok()?;

    let request = alloc::format!(
        "GET /xiaozhi/ota/ HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        server
    );
    write_all(&mut socket, request.as_bytes()).await.ok()?;

    let mut resp = heapless::String::<1024>::new();
    loop {
        let mut buf = [0u8; 256];
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => push_bytes(&mut resp, &buf[..n]),
        }
        if resp.len() >= resp.capacity() - 1 {
            break;
        }
    }
    socket.abort();

    // 跳过 HTTP 头取 body
    let body = match resp.find("\r\n\r\n") {
        Some(i) => &resp[i + 4..],
        None => {
            warn!("OTA: 响应无 body");
            return None;
        },
    };
    let manifest = ota::parse_manifest(body)?;
    let current = env!("CARGO_PKG_VERSION");
    let update_available =
        ota::compare_versions(manifest.version, current) == core::cmp::Ordering::Greater;
    let mut latest = heapless::String::<32>::new();
    let _ = latest.push_str(manifest.version);
    if update_available {
        info!("OTA: 有新版本 {} (当前 {})", latest.as_str(), current);
        let _ = manifest.url;
    } else {
        info!("OTA: 已是最新 ({})", current);
    }
    Some(UpdateReport {
        latest_version: latest,
        update_available,
    })
}

/// 后台任务: 开机 OTA 检查
///
/// `Stack` 是 `Copy` (embassy-net), 传入即复制一份, 与 `session_task` 各持
/// 一份互不干扰。独立任务跑 connect/读应答: 服务器可达时 <1s 打印版本比较
/// 结果; 不可达时 connect 自行超时, 不阻塞会话/主循环。
#[embassy_executor::task]
pub async fn check_task(stack: Stack<'static>, server: Ipv4Address) {
    if check(stack, server).await.is_none() {
        warn!("OTA 检查失败 (服务器 :8003 不可达或响应异常)");
    }
}

/// 逐字节 push 进 resp (容量满即停)
fn push_bytes(resp: &mut heapless::String<1024>, chunk: &[u8]) {
    for b in chunk {
        if resp.push(*b as char).is_err() {
            return;
        }
    }
}

async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        let n = socket
            .write_with(|buf| {
                let n = usize::min(buf.len(), data.len());
                buf[..n].copy_from_slice(&data[..n]);
                (n, n)
            })
            .await
            .map_err(|_| {
                error!("OTA: 请求发送失败");
            })?;
        if n == 0 {
            return Err(());
        }
        data = &data[n..];
    }
    Ok(())
}
