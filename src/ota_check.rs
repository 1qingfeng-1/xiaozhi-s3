//! OTA 检查 + 下载 + 烧写 (网络/flash 层)
//!
//! 开机由 [`check`] 同步调用 (main 直线路径, 采集未启动, 无 DMA 冲突):
//! POST :8003/xiaozhi/ota/ (Device-Id + JSON body 板型/版本) → 清单解析 →
//! 版本比较; 有新版时 [`download_and_flash`] GET 固件 → 流式 4KB 扇区
//! 擦写 → 写 otadata → 重启。对齐 C++ ota.cc 的下载/校验/烧写全流程。
//!
//! [`check_task`] 保留为后台任务 (M9 兼容): 独立任务, 不阻塞会话/主循环。

use embassy_net::{Ipv4Address, Stack, tcp::TcpSocket};
use embedded_storage::{ReadStorage as _, Storage as _};

use defmt::{error, info, warn};
use esp_storage::FlashStorage;

use xiaozhi_core::ota;

use crate::mk_static;

/// 检查结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// 服务器最新版本
    pub latest_version: heapless::String<32>,
    /// 是否比当前固件新
    pub update_available: bool,
    /// 固件下载 url (清单 firmware.url; 供 [`download_validate`])
    pub url: heapless::String<64>,
}

/// 固件下载 + 校验结果 (不写 flash)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReport {
    /// 新镜像里 app_desc 的版本串 (如 "0.2.0")
    pub version: heapless::String<32>,
    /// 实际读到的 body 字节数
    pub bytes: u32,
    /// 响应 Content-Length (0 = 未提供)
    pub expected: u32,
}

/// 板型标识 (服务器按 `board.type` 匹配 `data/bin/<type>_<version>.bin`;
/// 对齐 C++ `BOARD_TYPE` / config.json `name`)
const OTA_BOARD_TYPE: &str = "gezipai";

/// 向服务器查询固件更新
///
/// 对齐 C++ `ota.cc::CheckVersion`: **POST** 带 `Device-Id` 头 + JSON body
/// (`board.type` 板型 + `application.version` 当前版本); 服务器据此返回
/// `{firmware:{version,url}, ...}` 清单。GET 只回健康串、拿不到清单 (实测)。
/// `mac` 供 `Device-Id`/`Client-Id` 头 (服务器白名单/追踪用)。
pub async fn check(stack: Stack<'static>, server: Ipv4Address, mac: &str) -> Option<UpdateReport> {
    // check() 消费一次性 Stack<'static> (调用方 mk_static!), 故 rx/tx 亦用
    // mk_static! 常驻 (替代原 vec!().leak() 每次泄漏 1536B)
    let rx = mk_static!([u8; 1024], [0u8; 1024]);
    let tx = mk_static!([u8; 512], [0u8; 512]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.connect((server, 8003)).await.ok()?;

    let current = env!("CARGO_PKG_VERSION");
    let body = alloc::format!(
        "{{\"board\":{{\"type\":\"{}\"}},\"application\":{{\"version\":\"{}\"}}}}",
        OTA_BOARD_TYPE,
        current
    );
    let request = alloc::format!(
        "POST /xiaozhi/ota/ HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nDevice-Id: {}\r\nClient-Id: {}\r\nConnection: close\r\n\r\n{}",
        server,
        body.len(),
        mac,
        mac,
        body
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
    let mut url = heapless::String::<64>::new();
    let _ = url.push_str(manifest.url);
    if update_available {
        info!("OTA: 有新版本 {} (当前 {})", latest.as_str(), current);
    } else {
        info!("OTA: 已是最新 ({})", current);
    }
    Some(UpdateReport {
        latest_version: latest,
        update_available,
        url,
    })
}

/// 下载固件并校验 e9 头 (下载 → 校验 → 计数; **不写 flash**)
///
/// `url` = 清单里的 firmware.url; 连接用已知 `server` (端口取 url, 缺省 8003),
/// path 取 url `//` 之后的部分。`None` = 下载失败 / 镜像头校验失败 (magic 或
/// app_desc 不符, 即取回的坏固件, 拒绝)。
pub async fn download_validate(
    stack: Stack<'static>,
    server: Ipv4Address,
    url: &str,
) -> Option<ImageReport> {
    let (port, path) = parse_url(url);
    let rx = mk_static!([u8; 1024], [0u8; 1024]);
    let tx = mk_static!([u8; 1024], [0u8; 1024]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.connect((server, port)).await.ok()?;

    let request = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path,
        server
    );
    write_all(&mut socket, request.as_bytes()).await.ok()?;

    read_and_validate(&mut socket).await
}

// ===== M10: 下载 + 扇区级烧写 + otadata 切换 =====

/// ota_0 / ota_1 / otadata 分区基址 (与 partitions.csv 一致)
const OTA_0_BASE: u32 = 0x20_000;
const OTA_1_BASE: u32 = 0x41_0000;
const OTADATA_BASE: u32 = 0x10_000;
const SECTOR: u32 = 4096;

/// 下载固件 → 4KB 扇区擦写目标槽 → 写 otadata → 返回新版本串
///
/// 调用时机: `main()` 直线路径 (采集未启动, 无 I2S DMA 冲突)。
/// 成功时调用方负责 `esp_hal::system::software_reset()`。
/// 失败 (下载/校验/烧写) 返回 `None`, 设备继续用当前固件。
pub async fn download_and_flash(
    stack: Stack<'static>,
    server: Ipv4Address,
    url: &str,
    flash: &mut FlashStorage<'_>,
) -> Option<heapless::String<32>> {
    // 1. 读 otadata → 确定目标槽 (乒乓)
    let mut ota_buf = [0xFFu8; ota::OTADATA_PARTITION_SIZE as usize];
    flash.read(OTADATA_BASE, &mut ota_buf).ok()?;
    let entries = ota::read_otadata(&ota_buf);
    let (new_seq, target_slot) = ota::next_write(&entries, 2);
    let target_base = if target_slot == 0 {
        OTA_0_BASE
    } else {
        OTA_1_BASE
    };
    let target_name = if target_slot == 0 { "ota_0" } else { "ota_1" };
    info!("OTA: 烧写 → {} (seq {})", target_name, new_seq);

    // 2. HTTP GET
    let (port, path) = parse_url(url);
    let rx = mk_static!([u8; 4096], [0u8; 4096]);
    let tx = mk_static!([u8; 1024], [0u8; 1024]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.connect((server, port)).await.ok()?;
    let req = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path,
        server
    );
    write_all(&mut socket, req.as_bytes()).await.ok()?;

    // 3. 读 HTTP 头
    let mut head_buf = [0u8; 1024];
    let mut head_len = 0usize;
    let mut body_off: Option<usize> = None;
    loop {
        let n = match socket.read(&mut head_buf[head_len..]).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        head_len += n;
        if let Some(pos) = head_buf[..head_len]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            body_off = Some(pos + 4);
            break;
        }
        if head_len >= head_buf.len() {
            return None;
        }
    }
    let body_off = body_off?;
    let head_str = core::str::from_utf8(&head_buf[..head_len]).unwrap_or("");
    let expected = parse_content_length(head_str).unwrap_or(0);

    // 4. 读前 128B (e9 头) 校验
    let mut img_head = [0u8; 128];
    let mut n = 0usize;
    let rem = head_len - body_off;
    let cp = rem.min(128);
    img_head[..cp].copy_from_slice(&head_buf[body_off..body_off + cp]);
    n = cp;
    while n < 128 {
        let r = match socket.read(&mut img_head[n..]).await {
            Ok(0) | Err(_) => return None,
            Ok(r) => r,
        };
        n += r;
    }
    let Some(info) = ota::validate_image_header(&img_head) else {
        warn!("OTA: 镜像头校验失败 (magic/app_desc)");
        return None;
    };
    let mut version = heapless::String::<32>::new();
    let _ = version.push_str(info.version);
    info!("OTA: 镜像 {} , 开始烧写...", version.as_str());

    // 5. 流式 4KB 扇区写入
    let mut sector_buf = [0u8; SECTOR as usize];
    sector_buf[..128].copy_from_slice(&img_head);
    let mut fill = 128usize;
    let mut written = 0u32;
    let mut total = 128u32;
    let mut last_log = 0u32;

    loop {
        if fill < SECTOR as usize {
            let r = match socket.read(&mut sector_buf[fill..]).await {
                Ok(0) | Err(_) => break,
                Ok(r) => r,
            };
            fill += r;
        }
        while fill >= SECTOR as usize {
            let abs = target_base + written * SECTOR;
            if let Err(e) = flash.write(abs, &sector_buf[..SECTOR as usize]) {
                error!("OTA: 扇区写失败 @0x{:05X}: {:?}", abs, e);
                return None;
            }
            written += 1;
            total += SECTOR;
            sector_buf.copy_within(SECTOR as usize..fill, 0);
            fill -= SECTOR as usize;
            if total - last_log >= 64 * 1024 {
                let pct = if expected > 0 {
                    (total * 100) / expected
                } else {
                    0
                };
                info!("OTA: {}% ({}/{}KB)", pct, total / 1024, expected / 1024);
                last_log = total;
            }
        }
    }
    if fill > 0 {
        let mut last = [0xFFu8; SECTOR as usize];
        last[..fill].copy_from_slice(&sector_buf[..fill]);
        let abs = target_base + written * SECTOR;
        if let Err(e) = flash.write(abs, &last) {
            error!("OTA: 末扇区写失败 @0x{:05X}: {:?}", abs, e);
            return None;
        }
        written += 1;
        total += fill as u32;
    }
    socket.abort();

    // 6. 写 otadata 条目
    let entry = ota::OtaDataEntry::new(new_seq, version.as_str(), ota::OtaImageState::Valid);
    let entry_bytes = entry.to_bytes();
    let entry_abs = OTADATA_BASE + ota::otadata_entry_offset(target_slot);
    if let Err(e) = flash.write(entry_abs, &entry_bytes) {
        error!("OTA: otadata 写失败 @0x{:05X}: {:?}", entry_abs, e);
        return None;
    }

    info!(
        "OTA: 完成 ({}KB, {} 扇区), 重启进入 {}...",
        total / 1024,
        written,
        target_name
    );
    Some(version)
}

/// 流式读 HTTP 响应: 先读头 (到 `\r\n\r\n`, 取 Content-Length), 再读 body;
/// 前 128B body 用于 e9 头校验 (magic + app_desc → 版本, 需 ≥112B 覆盖
/// name@80..112), 其余只计数 (不落 RAM, 880KB 镜像不能整块进 128KB 堆)。
/// 校验通过返回 [`ImageReport`], 否则 `None`。
async fn read_and_validate(socket: &mut TcpSocket<'_>) -> Option<ImageReport> {
    let mut head = heapless::String::<1024>::new();
    let mut in_body = false;
    let mut img_head = [0u8; 128];
    let mut img_head_n = 0usize;
    let mut validated = false;
    let mut version = heapless::String::<32>::new();
    let mut total: u32 = 0;
    let mut expected: u32 = 0;

    loop {
        let mut buf = [0u8; 512];
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut i = 0usize;
                while i < n {
                    if !in_body {
                        if head.len() < head.capacity() {
                            let _ = head.push(buf[i] as char);
                        }
                        if head.ends_with("\r\n\r\n") {
                            in_body = true;
                            expected = parse_content_length(&head).unwrap_or(0);
                            // buf[i] 是 \r\n\r\n 的最后一个 \n, body 从 i+1 起
                            i += 1;
                            if i >= n {
                                break;
                            }
                            // fall through: 处理 buf[i..n] 为 body
                        } else {
                            i += 1;
                            continue;
                        }
                    }
                    if img_head_n < 128 {
                        img_head[img_head_n] = buf[i];
                        img_head_n += 1;
                        if img_head_n == 128 {
                            if let Some(info) = ota::validate_image_header(&img_head[..img_head_n])
                            {
                                validated = true;
                                version.clear();
                                let _ = version.push_str(info.version);
                            }
                        }
                    }
                    total += 1;
                    i += 1;
                }
            },
        }
    }

    if !in_body {
        warn!("OTA 下载: 响应无 body");
        return None;
    }
    if !validated {
        warn!("OTA 下载: 镜像头校验失败 (magic/app_desc 不符, 拒绝坏固件)");
        return None;
    }
    Some(ImageReport {
        version,
        bytes: total,
        expected,
    })
}

/// 从 url 取 (端口, 路径): 端口取 host:port (缺省 8003), 路径为 `//` 后到末尾
fn parse_url(url: &str) -> (u16, &str) {
    let after = url.find("://").map(|i| &url[i + 3..]).unwrap_or(url);
    let (hostport, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    let port = hostport
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8003);
    (port, path)
}

/// 从 HTTP 头取 Content-Length (大小写不敏感, 无拷贝; 缺省 `None`)
fn parse_content_length(head: &str) -> Option<u32> {
    let hl = head.as_bytes();
    let key = b"content-length:";
    if hl.len() < key.len() {
        return None;
    }
    let mut at = None;
    for i in 0..=(hl.len() - key.len()) {
        let mut ok = true;
        for j in 0..key.len() {
            if hl[i + j].to_ascii_lowercase() != key[j] {
                ok = false;
                break;
            }
        }
        if ok {
            at = Some(i + key.len());
            break;
        }
    }
    let at = at?;
    let v = head[at..].trim_start();
    let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    v[..end].parse().ok()
}

/// 后台任务: 开机 OTA 检查 (+ 有新版时下载校验)
///
/// `Stack` 是 `Copy` (embassy-net), 传入即复制一份, 与 `session_task` 各持
/// 一份互不干扰。独立任务跑 connect/读应答: 服务器可达时打印版本比较, 有新版
/// 时下载 + 校验 e9 头; 不可达时 connect 自行超时, 不阻塞会话/主循环。
#[embassy_executor::task]
pub async fn check_task(stack: Stack<'static>, server: Ipv4Address, mac: heapless::String<32>) {
    // 任务参数需 'static: 收 owned heapless::String (调用方 .clone()), 内部再借用
    match check(stack, server, &mac).await {
        Some(report) => {
            if !report.update_available {
                return;
            }
            info!("OTA: 下载校验固件 {} …", report.url.as_str());
            match download_validate(stack, server, report.url.as_str()).await {
                Some(img) => info!(
                    "OTA 下载校验通过: 镜像版本 {} 大小 {}B (Content-Length {})",
                    img.version.as_str(),
                    img.bytes,
                    img.expected
                ),
                None => warn!("OTA 下载校验失败 (无固件 / 镜像损坏 / 不可达)"),
            }
        },
        None => warn!("OTA 检查失败 (服务器 :8003 不可达或响应异常)"),
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
