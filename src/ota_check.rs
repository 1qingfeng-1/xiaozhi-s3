//! OTA 检查 + 下载 + 烧写 (网络/flash 层)
//!
//! 开机由 [`boot_ota_task`] 独立任务执行 (勿内联 main — 会膨胀 main future
//! .bss、压缩主栈, opus SILK ~87KB 编码会栈溢出):
//! POST :8003/xiaozhi/ota/ → 版本比较; 有新版时下载 → 4KB 扇区擦写 →
//! 写 otadata → 重启。对齐 C++ ota.cc 全流程。
//!
//! 防护: 分区上界 / HTTP 200 / Content-Length 完整性 / 45s 超时 /
//! 同版本失败≥3 次退避 (NVS 0xC000)。

use embassy_net::{Ipv4Address, Stack, tcp::TcpSocket};
use embassy_time::{Duration, with_timeout};
use embedded_storage::{ReadStorage as _, Storage as _};

use defmt::{error, info, warn};
use esp_storage::FlashStorage;

use xiaozhi_core::ota;

/// 检查结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// 服务器最新版本
    pub latest_version: heapless::String<32>,
    /// 是否比当前固件新
    pub update_available: bool,
    /// 固件下载 url (清单 firmware.url)
    pub url: heapless::String<160>,
}

/// 板型标识 (服务器按 `board.type` 匹配 `data/bin/<type>_<version>.bin`;
/// 对齐 C++ `BOARD_TYPE` / config.json `name`)
const OTA_BOARD_TYPE: &str = "gezipai";

/// ota_0 / ota_1 / otadata 分区基址与大小 (与 partitions.csv 一致)
const OTA_0_BASE: u32 = 0x20_000;
const OTA_1_BASE: u32 = 0x41_0000;
const OTADATA_BASE: u32 = 0x10_000;
/// 单 app 分区大小 (0x3F0000): 烧写硬上界, 防写穿相邻分区
const APP_PARTITION_SIZE: u32 = 0x3F_0000;
const SECTOR: u32 = 4096;

/// 网络阶段整体超时 (检查/下载); 防半开连接卡死启动路径
const NET_TIMEOUT: Duration = Duration::from_secs(45);

/// OTA 失败记录扇区 (NVS 分区内空闲扇区, 与凭据 0xE000 / 音量 0xD000 分离)
const OTA_FAIL_SECTOR: u32 = 0xC000;
const OTA_FAIL_MAGIC: u32 = 0x4154_4F58; // 'XOTA' 小端
/// 同版本连续失败达此次数后跳过 (跨重启生效)
const OTA_FAIL_LIMIT: u8 = 3;

type RawMutex = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

/// FlashStorage 裸指针 (单核独占: main 交出后等 done 才再用)
pub struct FlashPtr(pub *mut FlashStorage<'static>);
// SAFETY: 交接协议 — main spawn 后 await done, 任务结束前 main 不碰 flash
unsafe impl Send for FlashPtr {}

/// 开机 OTA 独立任务: 检查 → (有新版) 下载烧写 → 成功 reset / 失败通知 main
///
/// 状态机在本任务 TaskStorage, **不进 main future** (否则 .bss 挤掉主栈)。
#[embassy_executor::task]
pub async fn boot_ota_task(
    stack: Stack<'static>,
    server: Ipv4Address,
    mac: heapless::String<32>,
    flash_ptr: FlashPtr,
    done: embassy_sync::channel::Sender<'static, RawMutex, (), 1>,
) {
    let flash = unsafe { &mut *flash_ptr.0 };
    match check(stack, server, &mac).await {
        Some(report) if report.update_available => {
            if should_skip(flash, report.latest_version.as_str()) {
                // should_skip 内已告警
            } else {
                info!(
                    "OTA: 发现新版本 {}, 下载并烧写...",
                    report.latest_version.as_str()
                );
                match download_and_flash(stack, server, report.url.as_str(), flash).await {
                    Some(v) => {
                        info!("OTA: 烧写成功 ({}), 重启...", v.as_str());
                        esp_hal::system::software_reset();
                    },
                    None => {
                        error!("OTA: 烧写失败, 继续使用当前版本");
                        record_failure(flash, report.latest_version.as_str());
                    },
                }
            }
        },
        Some(_) => {}, // 已是最新
        None => warn!("OTA: 检查失败 (服务器不可达/超时/响应异常), 跳过本次升级"),
    }
    let _ = done.try_send(());
}

/// 向服务器查询固件更新
///
/// 对齐 C++ `ota.cc::CheckVersion`: **POST** 带 `Device-Id` 头 + JSON body。
pub async fn check(stack: Stack<'static>, server: Ipv4Address, mac: &str) -> Option<UpdateReport> {
    with_timeout(NET_TIMEOUT, check_inner(stack, server, mac))
        .await
        .ok()?
}

async fn check_inner(
    stack: Stack<'static>,
    server: Ipv4Address,
    mac: &str,
) -> Option<UpdateReport> {
    // 大缓冲走堆泄漏: 勿用 mk_static (挤 .bss → 压缩主栈) 勿用栈 (挤 opus)
    let rx: &'static mut [u8] = alloc::vec![0u8; 1024].leak();
    let tx: &'static mut [u8] = alloc::vec![0u8; 512].leak();
    let mut socket = TcpSocket::new(stack, rx, tx);
    if socket.connect((server, 8003)).await.is_err() {
        warn!("OTA: TCP 连接失败");
        return None;
    }

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

    let resp: &'static mut heapless::String<1024> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(heapless::String::new()));
    let mut read_buf = [0u8; 256];
    loop {
        match socket.read(&mut read_buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => push_bytes(resp, &read_buf[..n]),
        }
        if resp.len() >= resp.capacity() - 1 {
            break;
        }
    }
    socket.abort();

    if resp.is_empty() {
        warn!("OTA: 检查响应为空 (服务器关闭连接)");
        return None;
    }
    let (status, body) = split_http_response(resp.as_str())?;
    if status != 200 {
        warn!("OTA: 检查响应非 200 ({})", status);
        return None;
    }
    let Some(manifest) = ota::parse_manifest(body) else {
        warn!("OTA: 清单解析失败 (body 前 80B)");
        return None;
    };
    let current = env!("CARGO_PKG_VERSION");
    let update_available =
        ota::compare_versions(manifest.version, current) == core::cmp::Ordering::Greater;
    let mut latest = heapless::String::<32>::new();
    let _ = latest.push_str(manifest.version);
    let mut url = heapless::String::<160>::new();
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

/// 下载固件 → 4KB 扇区擦写目标槽 → 写 otadata → 返回新版本串
///
/// 防护: HTTP 200 / Content-Length 完整性 / 分区大小上界 / 整体超时。
/// 调用时机: `main()` 直线路径 (采集未启动, 无 I2S DMA 冲突)。
/// 成功时调用方负责 `esp_hal::system::software_reset()`。
/// 失败返回 `None`, 设备继续用当前固件; 失败计数由调用方 [`record_failure`]。
pub async fn download_and_flash(
    stack: Stack<'static>,
    server: Ipv4Address,
    url: &str,
    flash: &mut FlashStorage<'_>,
) -> Option<heapless::String<32>> {
    with_timeout(
        NET_TIMEOUT,
        download_and_flash_inner(stack, server, url, flash),
    )
    .await
    .ok()?
}

async fn download_and_flash_inner(
    stack: Stack<'static>,
    server: Ipv4Address,
    url: &str,
    flash: &mut FlashStorage<'_>,
) -> Option<heapless::String<32>> {
    // 1. 读 otadata → 确定目标槽 (乒乓)
    // 只读两条 32B 条目: 勿用 8KB 静态缓冲 (挤占主任务栈 = RWDATA 剩余)
    let mut e0 = [0u8; 32];
    let mut e1 = [0u8; 32];
    flash.read(OTADATA_BASE, &mut e0).ok()?;
    flash
        .read(OTADATA_BASE + ota::otadata_entry_offset(1), &mut e1)
        .ok()?;
    let entries = [
        ota::OtaDataEntry::from_bytes(&e0),
        ota::OtaDataEntry::from_bytes(&e1),
    ];
    let (new_seq, target_slot) = ota::next_write(&entries, 2);
    let target_base = if target_slot == 0 {
        OTA_0_BASE
    } else {
        OTA_1_BASE
    };
    let target_name = if target_slot == 0 { "ota_0" } else { "ota_1" };
    info!("OTA: 烧写 → {} (seq {})", target_name, new_seq);

    // 2. HTTP GET
    let (url_host, port, path) = parse_url(url);
    let host = url_host.unwrap_or(server);
    let rx: &'static mut [u8] = alloc::vec![0u8; 4096].leak();
    let tx: &'static mut [u8] = alloc::vec![0u8; 1024].leak();
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.connect((host, port)).await.ok()?;
    let req = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path,
        host
    );
    info!("OTA GET {}", req.as_str());
    write_all(&mut socket, req.as_bytes()).await.ok()?;

    // 3. 读 HTTP 头 + 校验状态码
    let head_buf: &'static mut [u8] = alloc::vec![0u8; 1024].leak();
    let mut head_len = 0usize;
    let body_off;
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
            body_off = pos + 4;
            break;
        }
        if head_len >= head_buf.len() {
            return None;
        }
    }
    let head_str = core::str::from_utf8(&head_buf[..head_len]).unwrap_or("");
    let status = parse_http_status(head_str)?;
    if status != 200 {
        warn!("OTA: 下载响应非 200 ({})", status);
        return None;
    }
    // Content-Length 必填: 完整性依赖它 (缺失无法证明镜像完整)
    let expected = parse_content_length(head_str)?;
    if expected == 0 || expected > APP_PARTITION_SIZE {
        warn!(
            "OTA: Content-Length 非法 ({} > 分区 {} 或为 0)",
            expected, APP_PARTITION_SIZE
        );
        return None;
    }

    // 4. 读前 128B (e9 头) 校验
    let mut img_head = [0u8; 128];
    let rem = head_len - body_off;
    let cp = rem.min(128);
    img_head[..cp].copy_from_slice(&head_buf[body_off..body_off + cp]);
    let mut n = cp;
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
    info!("OTA: 镜像 {}, 开始烧写...", version.as_str());

    // 5. 流式 4KB 扇区写入 (硬上界 = APP_PARTITION_SIZE)
    let sector_buf: &'static mut [u8] = alloc::vec![0u8; 4096].leak();
    sector_buf[..128].copy_from_slice(&img_head);
    let mut fill = 128usize;
    let mut written = 0u32;
    // 扇区计数已包含前 128B 镜像头, 不能从 128 起算 (会多算 128B)
    let mut total = 0u32;
    let mut last_log = 0u32;

    loop {
        if fill < 4096 {
            let r = match socket.read(&mut sector_buf[fill..]).await {
                Ok(0) | Err(_) => break,
                Ok(r) => r,
            };
            fill += r;
        }
        while fill >= 4096 {
            // 分区上界: 写满前一扇区后下一地址不得越过 APP_PARTITION_SIZE
            if written * SECTOR >= APP_PARTITION_SIZE {
                error!("OTA: 镜像超出分区 {}B, 拒绝写入", APP_PARTITION_SIZE);
                return None;
            }
            let abs = target_base + written * SECTOR;
            if let Err(e) = flash.write(abs, &sector_buf[..4096]) {
                error!("OTA: 扇区写失败 @0x{:05X}: {:?}", abs, e);
                return None;
            }
            written += 1;
            total += SECTOR;
            sector_buf.copy_within(4096..fill, 0);
            fill -= 4096;
            if total - last_log >= 64 * 1024 {
                let pct = (total * 100) / expected;
                info!("OTA: {}% ({}/{}KB)", pct, total / 1024, expected / 1024);
                last_log = total;
            }
        }
    }
    if fill > 0 {
        if written * SECTOR >= APP_PARTITION_SIZE {
            error!("OTA: 镜像超出分区 {}B, 拒绝写入", APP_PARTITION_SIZE);
            return None;
        }
        // 末扇区: 只写实际字节 (Storage 会整扇区擦写, 无需单独 4KB 缓冲)
        let abs = target_base + written * SECTOR;
        if let Err(e) = flash.write(abs, &sector_buf[..fill]) {
            error!("OTA: 末扇区写失败 @0x{:05X}: {:?}", abs, e);
            return None;
        }
        written += 1;
        total += fill as u32;
    }
    socket.abort();

    // 完整性: 实收必须等于 Content-Length (截断/超长均拒绝, 防半截镜像被 VALID)
    if total != expected {
        warn!(
            "OTA: 镜像不完整 (收到 {}B != Content-Length {}B), 拒绝切槽",
            total, expected
        );
        return None;
    }

    // 6. 擦除目标 otadata 扇区并写 PENDING_VERIFY 条目
    //    新镜像首次启动成功、WiFi 连上后才由 confirm_running_image 置 VALID;
    //    未确认时由二级引导器按 PENDING_VERIFY 回滚到旧槽。
    let entry =
        ota::OtaDataEntry::new(new_seq, version.as_str(), ota::OtaImageState::PendingVerify);
    let entry_bytes = entry.to_bytes();
    let entry_abs = OTADATA_BASE + ota::otadata_entry_offset(target_slot);
    let otadata_sector = OTADATA_BASE + target_slot as u32 * SECTOR;
    if let Err(e) = embedded_storage::nor_flash::NorFlash::erase(flash, otadata_sector, SECTOR) {
        error!(
            "OTA: otadata 扇区擦除失败 @0x{:05X}: {:?}",
            otadata_sector, e
        );
        return None;
    }
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

/// 是否应跳过该版本的 OTA 尝试 (同版本连续失败 ≥ [`OTA_FAIL_LIMIT`] 次)
pub fn should_skip(flash: &mut FlashStorage<'_>, version: &str) -> bool {
    match load_fail_record(flash) {
        Some((v, count)) if count >= OTA_FAIL_LIMIT && v == version => {
            warn!("OTA: 版本 {} 已连续失败 {} 次, 本次跳过", version, count);
            true
        },
        _ => false,
    }
}

/// 记录一次该版本的烧写失败 (跨重启累计; 成功路径会复位, 无需清零)
pub fn record_failure(flash: &mut FlashStorage<'_>, version: &str) {
    let (prev_ver, prev_count) = load_fail_record(flash).unwrap_or((heapless::String::new(), 0));
    let count = if prev_ver.as_str() == version {
        prev_count.saturating_add(1)
    } else {
        1
    };
    save_fail_record(flash, version, count);
}

fn load_fail_record(flash: &mut FlashStorage<'_>) -> Option<(heapless::String<32>, u8)> {
    let mut buf = [0u8; 40];
    flash.read(OTA_FAIL_SECTOR, &mut buf).ok()?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    if magic != OTA_FAIL_MAGIC {
        return None;
    }
    let count = buf[36];
    if count == 0 || count == 0xFF {
        return None;
    }
    let ver = core::str::from_utf8(&buf[4..36]).ok()?;
    let end = ver.find('\0').unwrap_or(ver.len());
    let mut s = heapless::String::<32>::new();
    s.push_str(&ver[..end]).ok()?;
    Some((s, count))
}

fn save_fail_record(flash: &mut FlashStorage<'_>, version: &str, count: u8) {
    // 只写 40B 记录, 不要 4KB 静态 (主任务栈吃紧)
    let mut buf = [0xFFu8; 40];
    buf[0..4].copy_from_slice(&OTA_FAIL_MAGIC.to_le_bytes());
    let n = version.len().min(31);
    buf[4..4 + n].copy_from_slice(&version.as_bytes()[..n]);
    buf[36] = count;
    // Storage::write 对非扇区对齐会整扇区 RMW, 40B 足够
    if let Err(e) = flash.write(OTA_FAIL_SECTOR, &buf) {
        error!("OTA: 失败计数写入失败: {:?}", e);
    }
}

/// 拆分 HTTP 响应为 (状态码, body); 无头/非 HTTP 返回 None
fn split_http_response(resp: &str) -> Option<(u16, &str)> {
    let header_end = resp.find("\r\n\r\n")?;
    let status = parse_http_status(&resp[..header_end + 4])?;
    Some((status, &resp[header_end + 4..]))
}

/// 从 HTTP 状态行取状态码 (`HTTP/1.x 200 ...`)
fn parse_http_status(head: &str) -> Option<u16> {
    let line_end = head.find("\r\n").unwrap_or(head.len());
    let line = &head[..line_end];
    let mut parts = line.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// 从 url 取 (端口, 路径): 端口取 host:port (缺省 8003), 路径为 `//` 后到末尾
fn parse_url(url: &str) -> (Option<Ipv4Address>, u16, &str) {
    let after = url.find("://").map(|i| &url[i + 3..]).unwrap_or(url);
    let (hostport, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    // 仅支持 IPv4 字面量主机; 域名无 DNS 解析, 回退到配置 server
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h, port),
            Err(_) => (hostport, 8003),
        },
        None => (hostport, 8003),
    };
    (host.parse().ok(), port, path)
}

/// 从 HTTP 头取 Content-Length (大小写不敏感; 缺省 `None`)
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

/// 启动确认: 若当前活动条目为 PENDING_VERIFY, 在 WiFi 连通后写 VALID 确认。
///
/// 成功返回确认的 seq; 无待确认条目返回 None。
pub fn confirm_running_image(flash: &mut FlashStorage<'static>) -> Option<u32> {
    let mut buf = [0xFFu8; 64];
    flash.read(OTADATA_BASE, &mut buf).ok()?;
    let e0 = ota::OtaDataEntry::from_bytes(&buf[..32]);
    let e1 = ota::OtaDataEntry::from_bytes(&buf[32..]);
    let (slot, entry) = if e0.is_pending() {
        (0usize, e0)
    } else if e1.is_pending() {
        (1usize, e1)
    } else {
        return None;
    };
    let other = 1 - slot;
    let abs = OTADATA_BASE + other as u32 * SECTOR;
    if embedded_storage::nor_flash::NorFlash::erase(flash, abs, SECTOR).is_err() {
        error!("OTA: 确认镜像时擦除 otadata 失败");
        return None;
    }
    let valid = ota::OtaDataEntry::new(entry.ota_seq, entry.label_str(), ota::OtaImageState::Valid);
    if flash.write(abs, &valid.to_bytes()).is_err() {
        error!("OTA: 确认镜像写 VALID 失败");
        return None;
    }
    info!("OTA: 已确认运行镜像 seq={}", entry.ota_seq);
    Some(entry.ota_seq)
}
