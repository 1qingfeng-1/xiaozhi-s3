//! 配网凭据持久化 — 写入 Flash NVS 分区的最后一个 4KB 扇区
//!
//! 布局: magic u32 | ssid_len u16 | ssid | pass_len u16 | pass |
//! server_len u16 | server。写入前整扇区擦除。

use embedded_storage::ReadStorage as _;
use embedded_storage::nor_flash::NorFlash;
use esp_storage::FlashStorage;

use super::WifiConfig;

/// NVS 分区 (0x9000..0xF000) 的最后一个扇区
const SECTOR: u32 = 0xE000;
const SECTOR_SIZE: u32 = 4096;
const MAGIC: u32 = 0x5849_4F5A; // 'ZOIX' 小端

/// 保存配置 (整扇区擦写)
pub fn save(flash: &mut FlashStorage<'_>, config: &WifiConfig) -> Result<(), &'static str> {
    let mut buf = [0u8; SECTOR_SIZE as usize];
    let fields = [
        config.ssid.as_str(),
        config.password.as_str(),
        config.server.as_str(),
    ];
    // 编码失败只可能是字段超长/容量不足; 写整扇区 (尾部 0) 保持旧格式
    xiaozhi_core::config_store::encode(MAGIC, &fields, &mut buf)
        .ok_or("配置字段过长或超出存储容量")?;
    NorFlash::erase(flash, SECTOR, SECTOR + SECTOR_SIZE).map_err(|_| "Flash 擦除失败")?;
    NorFlash::write(flash, SECTOR, &buf).map_err(|_| "Flash 写入失败")
}

/// 读取配置 (无有效记录返回 None)
pub fn load(flash: &mut FlashStorage<'_>) -> Option<WifiConfig> {
    let mut buf = [0u8; SECTOR_SIZE as usize];
    flash.read(SECTOR, &mut buf).ok()?;

    // 旧格式 (无 CRC) 末尾为 0, legacy_zero_crc=true 兼容
    let mut fields: [&str; 3] = ["", "", ""];
    if xiaozhi_core::config_store::decode(MAGIC, &buf, &mut fields, true).is_none() {
        let h = u32::from_le_bytes(buf[..4].try_into().unwrap_or([0; 4]));
        defmt::error!("凭据无效或损坏: 扇区头=0x{:08X} (期望 0x{:08X})", h, MAGIC);
        return None;
    }
    Some(WifiConfig {
        ssid: heapless::String::try_from(fields[0]).ok()?,
        password: heapless::String::try_from(fields[1]).ok()?,
        server: heapless::String::try_from(fields[2]).ok()?,
    })
}

/// 清除配置 (连接失败时调用, 回到配网模式)
pub fn clear(flash: &mut FlashStorage<'_>) -> Result<(), &'static str> {
    NorFlash::erase(flash, SECTOR, SECTOR + SECTOR_SIZE).map_err(|_| "Flash 擦除失败")
}

/// 音量持久化 — NVS 分区独立扇区 (0xD000, 与凭据 0xE000 分离)
const VOLUME_SECTOR: u32 = 0xD000;
const VOLUME_SECTOR_SIZE: u32 = 4096;
/// 单条记录 4 字节 (首字节音量), 按 4B 对齐追加, 避免整扇区擦除
const VOLUME_RECORD: usize = 4;

/// 下一条可编程记录的偏移 (0 = 尚未扫描)
static VOLUME_NEXT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);

/// 扫描扇区: 返回 (最后一条有效音量, 下一条空闲记录偏移)
///
/// 记录按 4B 槽追加; 首字节 0xFF 视为空闲槽。旧格式 (首字节音量 +
/// 其余 0xFF) 天然兼容: 扫描到槽 1 时返回音量并定位到偏移 4。
fn scan_volume(flash: &mut FlashStorage<'_>) -> (u8, u16) {
    let mut buf = [0u8; 64];
    let mut last = 60u8;
    let mut off = 0u32;
    while off < VOLUME_SECTOR_SIZE {
        if flash.read(VOLUME_SECTOR + off, &mut buf).is_err() {
            return (last, off as u16);
        }
        let mut i = 0usize;
        while i + VOLUME_RECORD <= buf.len() {
            if buf[i] == 0xFF {
                return (last, (off as usize + i) as u16);
            }
            last = buf[i].min(100);
            i += VOLUME_RECORD;
        }
        off += buf.len() as u32;
    }
    (last, VOLUME_SECTOR_SIZE as u16)
}

/// 保存音量 (0~100): 追加一条 4B 记录, 不擦扇区
///
/// 旧实现每次调节都擦 4KB (25–100ms), 而调用点在音频主循环里。
/// 追加写只编程 4B, 扇区写满 (约 1024 次调节) 才擦除一次。
pub fn save_volume(flash: &mut FlashStorage<'_>, volume: u8) {
    use core::sync::atomic::Ordering;
    let v = volume.min(100);
    let mut next = VOLUME_NEXT.load(Ordering::Relaxed);
    if next == 0 {
        // 尚未 load_volume (或首次写入): 先定位空闲槽
        let (_, n) = scan_volume(flash);
        next = n;
    }
    if next as u32 + VOLUME_RECORD as u32 > VOLUME_SECTOR_SIZE {
        if let Err(e) = NorFlash::erase(flash, VOLUME_SECTOR, VOLUME_SECTOR + VOLUME_SECTOR_SIZE) {
            defmt::error!("音量保存: 擦除失败 {:?}", e);
            return;
        }
        next = 0;
    }
    let word = [v, 0xFF, 0xFF, 0xFF];
    if let Err(e) = NorFlash::write(flash, VOLUME_SECTOR + next as u32, &word) {
        defmt::error!("音量保存: 写入失败 {:?}", e);
        return;
    }
    VOLUME_NEXT.store(next + VOLUME_RECORD as u16, Ordering::Relaxed);
}

/// 读取音量 (无记录/擦除态返回默认 60)
pub fn load_volume(flash: &mut FlashStorage<'_>) -> u8 {
    use core::sync::atomic::Ordering;
    let (v, next) = scan_volume(flash);
    VOLUME_NEXT.store(next, Ordering::Relaxed);
    v
}

/// xiaozhi 协议版本 — NVS 独立扇区 (0xB000, 与音量/凭据分离)
///
/// 0xB000 首字节: 1 = 裸 opus (默认), 2 = 16B 头含 AEC 时间戳,
/// 3 = 4B 头。2/3 需要服务端支持对应帧格式 (MQTT/UDP 网关或新版 WS 服务端)。
const PROTOCOL_SECTOR: u32 = 0xB000;

/// 读取协议版本 (无记录/擦除态 → 1)
pub fn load_ws_version(flash: &mut FlashStorage<'_>) -> u16 {
    let mut buf = [0u8; 1];
    match flash.read(PROTOCOL_SECTOR, &mut buf) {
        Ok(()) if (1..=3).contains(&buf[0]) => buf[0] as u16,
        _ => 1,
    }
}

/// 保存协议版本 (1..=3)
pub fn save_ws_version(flash: &mut FlashStorage<'_>, version: u16) -> Result<(), &'static str> {
    if !(1..=3).contains(&version) {
        return Err("协议版本超出范围");
    }
    let word = [version as u8, 0xFF, 0xFF, 0xFF];
    NorFlash::erase(flash, PROTOCOL_SECTOR, PROTOCOL_SECTOR + 4096)
        .map_err(|_| "Flash 擦除失败")?;
    NorFlash::write(flash, PROTOCOL_SECTOR, &word).map_err(|_| "Flash 写入失败")
}

/// MQTT 网关配置 (OTA 下发; 存在时设备改走 MQTT+UDP 传输)
#[derive(Clone, PartialEq, Eq)]
pub struct MqttSettings {
    /// `host:port` (网关 MQTT broker)
    pub endpoint: heapless::String<64>,
    pub client_id: heapless::String<96>,
    pub username: heapless::String<96>,
    pub password: heapless::String<96>,
    pub publish_topic: heapless::String<32>,
    pub subscribe_topic: heapless::String<64>,
}

impl MqttSettings {
    fn fields(&self) -> [&str; 6] {
        [
            self.endpoint.as_str(),
            self.client_id.as_str(),
            self.username.as_str(),
            self.password.as_str(),
            self.publish_topic.as_str(),
            self.subscribe_topic.as_str(),
        ]
    }
}

/// MQTT 配置扇区 (0xA000, NVS 分区内独立扇区)
const MQTT_SECTOR: u32 = 0xA000;
const MQTT_MAGIC: u32 = 0x4643_514D; // 'MQCF' 小端

/// 保存 MQTT 配置 (整扇区擦写; 内容未变不擦写)
pub fn save_mqtt(flash: &mut FlashStorage<'_>, cfg: &MqttSettings) -> Result<(), &'static str> {
    // 内容未变: 不重复擦写 (OTA 每次开机都会下发同一份配置)
    if load_mqtt(flash).as_ref() == Some(cfg) {
        return Ok(());
    }
    // 写整扇区: esp-storage 要求写入长度 4B 对齐, 只写 record 长度会在
    // 字段串长度之和 mod 4 != 0 时返回 NotAligned (与 save_websocket 同因)。
    let mut buf = [0u8; SECTOR_SIZE as usize];
    let fields = cfg.fields();
    xiaozhi_core::config_store::encode(MQTT_MAGIC, &fields, &mut buf).ok_or("MQTT 配置超出容量")?;
    NorFlash::erase(flash, MQTT_SECTOR, MQTT_SECTOR + SECTOR_SIZE).map_err(|_| "Flash 擦除失败")?;
    NorFlash::write(flash, MQTT_SECTOR, &buf).map_err(|_| "Flash 写入失败")
}

/// 读取 MQTT 配置 (无记录/损坏 → None)
pub fn load_mqtt(flash: &mut FlashStorage<'_>) -> Option<MqttSettings> {
    let mut buf = [0u8; 512];
    flash.read(MQTT_SECTOR, &mut buf).ok()?;
    // 旧格式 (末尾 0xFF/无 CRC) 一律拒绝 → 回退 WS
    let mut fields: [&str; 6] = ["", "", "", "", "", ""];
    xiaozhi_core::config_store::decode(MQTT_MAGIC, &buf, &mut fields, false)?;
    Some(MqttSettings {
        endpoint: heapless::String::try_from(fields[0]).ok()?,
        client_id: heapless::String::try_from(fields[1]).ok()?,
        username: heapless::String::try_from(fields[2]).ok()?,
        password: heapless::String::try_from(fields[3]).ok()?,
        publish_topic: heapless::String::try_from(fields[4]).ok()?,
        subscribe_topic: heapless::String::try_from(fields[5]).ok()?,
    })
}

/// 清除 MQTT 配置 (endpoint 非法/损坏时回退 WS)
pub fn clear_mqtt(flash: &mut FlashStorage<'_>) -> Result<(), &'static str> {
    NorFlash::erase(flash, MQTT_SECTOR, MQTT_SECTOR + 4096).map_err(|_| "Flash 擦除失败")
}

/// WebSocket 连接参数 (OTA 下发; 存在时优先于默认 WS 地址)
#[derive(Clone, PartialEq, Eq)]
pub struct WebsocketSettings {
    /// `ws://<ipv4>[:port][/path]`
    pub url: heapless::String<160>,
    /// 原始 token (不含 `Bearer ` 前缀, 握手时统一补)
    pub token: heapless::String<96>,
    /// xiaozhi 协议版本 (1/2/3)
    pub version: u8,
}

impl WebsocketSettings {
    fn fields<'a>(&'a self, version: &'a mut heapless::String<3>) -> [&'a str; 3] {
        use core::fmt::Write as _;
        version.clear();
        let _ = write!(version, "{}", self.version);
        [self.url.as_str(), self.token.as_str(), version.as_str()]
    }
}

/// WebSocket 配置扇区 (0x9000, NVS 分区首扇区, 与 MQTT 0xA000 分离)
const WS_CONFIG_SECTOR: u32 = 0x9000;
const WS_CONFIG_MAGIC: u32 = 0x4643_5357; // 'WSCF' 小端

/// 保存 WebSocket 配置 (整扇区擦写; 内容未变不擦写)
pub fn save_websocket(
    flash: &mut FlashStorage<'_>,
    cfg: &WebsocketSettings,
) -> Result<(), &'static str> {
    if !(1..=3).contains(&cfg.version) {
        return Err("WS 协议版本超出范围");
    }
    // 内容未变: 不重复擦写 (OTA 每次开机都会下发同一份配置)
    if load_websocket(flash).as_ref() == Some(cfg) {
        return Ok(());
    }
    // 写整扇区: 部分 flash 驱动要求写入长度 4B 对齐, 只写 record 长度
    // 会返回 "Flash 写入失败" (真机 OTA 下发 WS 配置时已复现)。
    let mut buf = [0u8; SECTOR_SIZE as usize];
    let mut version = heapless::String::<3>::new();
    let fields = cfg.fields(&mut version);
    xiaozhi_core::config_store::encode(WS_CONFIG_MAGIC, &fields, &mut buf)
        .ok_or("WebSocket 配置超出容量")?;
    NorFlash::erase(flash, WS_CONFIG_SECTOR, WS_CONFIG_SECTOR + SECTOR_SIZE)
        .map_err(|_| "Flash 擦除失败")?;
    NorFlash::write(flash, WS_CONFIG_SECTOR, &buf).map_err(|_| "Flash 写入失败")
}

/// 读取 WebSocket 配置 (无记录/损坏/版本非法 → None)
pub fn load_websocket(flash: &mut FlashStorage<'_>) -> Option<WebsocketSettings> {
    let mut buf = [0u8; 512];
    flash.read(WS_CONFIG_SECTOR, &mut buf).ok()?;
    let mut fields: [&str; 3] = ["", "", ""];
    xiaozhi_core::config_store::decode(WS_CONFIG_MAGIC, &buf, &mut fields, false)?;
    let version: u8 = fields[2].parse().ok()?;
    if !(1..=3).contains(&version) {
        return None;
    }
    Some(WebsocketSettings {
        url: heapless::String::try_from(fields[0]).ok()?,
        token: heapless::String::try_from(fields[1]).ok()?,
        version,
    })
}

/// 启动自检: 写已知模式到 0x9000 并读回, 验证存储层工作正常
///
/// 0x9000 现在存放 WebSocket 配置: 自检前后读回/恢复原内容,
/// 避免调试 bin 把 OTA 下发的配置擦掉。
pub fn self_test(flash: &mut FlashStorage<'_>) -> bool {
    const TEST_SECTOR: u32 = WS_CONFIG_SECTOR;
    let mut saved = alloc::vec![0xFFu8; SECTOR_SIZE as usize];
    if flash.read(TEST_SECTOR, &mut saved).is_err() {
        defmt::error!("存储自检: 读取原内容失败");
        return false;
    }
    let mut w = [0u8; 256];
    for (i, b) in w.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    if NorFlash::erase(flash, TEST_SECTOR, TEST_SECTOR + SECTOR_SIZE).is_err() {
        defmt::error!("存储自检: 擦除失败");
        return false;
    }
    if NorFlash::write(flash, TEST_SECTOR, &w).is_err() {
        defmt::error!("存储自检: 写入失败");
        return false;
    }
    let mut r = [0u8; 256];
    if flash.read(TEST_SECTOR, &mut r).is_err() {
        defmt::error!("存储自检: 读取失败");
        return false;
    }
    let ok = w == r;
    // 恢复原内容 (即使自检失败也尝试, 失败只能重新 OTA 下发)
    let restored = NorFlash::erase(flash, TEST_SECTOR, TEST_SECTOR + SECTOR_SIZE).is_ok()
        && NorFlash::write(flash, TEST_SECTOR, &saved).is_ok();
    if !restored {
        defmt::error!("存储自检: 原内容恢复失败");
        return false;
    }
    if ok {
        defmt::info!("存储自检: 通过");
    } else {
        defmt::error!("存储自检: 读回数据不一致!");
    }
    ok
}
