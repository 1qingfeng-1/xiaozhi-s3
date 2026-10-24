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
    buf[..4].copy_from_slice(&MAGIC.to_le_bytes());

    let mut o = 4usize;
    for part in [
        config.ssid.as_bytes(),
        config.password.as_bytes(),
        config.server.as_bytes(),
    ] {
        let len = u16::try_from(part.len()).map_err(|_| "配置字段过长")?;
        buf[o..o + 2].copy_from_slice(&len.to_le_bytes());
        o += 2;
        if o + part.len() > SECTOR_SIZE as usize {
            return Err("配置超出存储容量");
        }
        buf[o..o + part.len()].copy_from_slice(part);
        o += part.len();
    }

    NorFlash::erase(flash, SECTOR, SECTOR + SECTOR_SIZE).map_err(|_| "Flash 擦除失败")?;
    NorFlash::write(flash, SECTOR, &buf).map_err(|_| "Flash 写入失败")
}

/// 读取配置 (无有效记录返回 None)
pub fn load(flash: &mut FlashStorage<'_>) -> Option<WifiConfig> {
    let mut buf = [0u8; SECTOR_SIZE as usize];
    flash.read(SECTOR, &mut buf).ok()?;

    let magic = u32::from_le_bytes(buf[..4].try_into().ok()?);
    if magic != MAGIC {
        let h0 = u32::from_le_bytes(buf[..4].try_into().unwrap_or([0; 4]));
        let h1 = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4]));
        defmt::error!(
            "凭据无效: 扇区头=0x{:08X} 0x{:08X} (期望 magic=0x{:08X})",
            h0,
            h1,
            MAGIC
        );
        return None;
    }

    let mut o = 4usize;
    let mut next = |_n: usize| -> Option<&[u8]> {
        let len = u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?) as usize;
        o += 2;
        let out = buf.get(o..o + len)?;
        o += len;
        Some(out)
    };

    let ssid = next(32)?;
    let password = next(64)?;
    let server = next(64)?;

    Some(WifiConfig {
        ssid: heapless::String::try_from(core::str::from_utf8(ssid).ok()?).ok()?,
        password: heapless::String::try_from(core::str::from_utf8(password).ok()?).ok()?,
        server: heapless::String::try_from(core::str::from_utf8(server).ok()?).ok()?,
    })
}

/// 清除配置 (连接失败时调用, 回到配网模式)
pub fn clear(flash: &mut FlashStorage<'_>) -> Result<(), &'static str> {
    NorFlash::erase(flash, SECTOR, SECTOR + SECTOR_SIZE).map_err(|_| "Flash 擦除失败")
}

/// 音量持久化 — NVS 分区独立扇区 (0xD000, 与凭据 0xE000 分离)
const VOLUME_SECTOR: u32 = 0xD000;

/// 保存音量 (0~100; 0xFF = 擦除态 → 默认 60)
pub fn save_volume(flash: &mut FlashStorage<'_>, volume: u8) {
    // 只写首字节, 避免 4KB 栈缓冲 (调用点在音频主循环, 编码栈已 ~87KB)
    let word = [volume.min(100), 0xFF, 0xFF, 0xFF];
    if let Err(e) = NorFlash::erase(flash, VOLUME_SECTOR, VOLUME_SECTOR + 4096) {
        defmt::error!("音量保存: 擦除失败 {:?}", e);
        return;
    }
    if let Err(e) = NorFlash::write(flash, VOLUME_SECTOR, &word) {
        defmt::error!("音量保存: 写入失败 {:?}", e);
    }
}

/// 读取音量 (无记录/擦除态返回默认 60)
pub fn load_volume(flash: &mut FlashStorage<'_>) -> u8 {
    let mut buf = [0u8; 1];
    match flash.read(VOLUME_SECTOR, &mut buf) {
        Ok(()) if buf[0] != 0xFF => buf[0].min(100),
        _ => 60,
    }
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
    /// 各字段容量 (序列化/反序列化共用): endpoint/client_id/username/
    /// password/publish_topic/subscribe_topic
    const CAPS: [usize; 6] = [64, 96, 96, 96, 32, 64];

    /// 序列化后的总长度上界 (magic 4 + 6×(len 2 + 容量))
    const BUF_LEN: usize = 4 + 6 * 2 + 64 + 96 + 96 + 96 + 32 + 64;

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

/// 保存 MQTT 配置 (整扇区擦写; 仅在内容变化时调用)
pub fn save_mqtt(flash: &mut FlashStorage<'_>, cfg: &MqttSettings) -> Result<(), &'static str> {
    let mut buf = [0u8; 512];
    if buf.len() < MqttSettings::BUF_LEN {
        return Err("MQTT 配置缓冲不足");
    }
    buf[..4].copy_from_slice(&MQTT_MAGIC.to_le_bytes());
    let mut o = 4usize;
    for s in cfg.fields() {
        let len = u16::try_from(s.len()).map_err(|_| "MQTT 字段过长")?;
        buf[o..o + 2].copy_from_slice(&len.to_le_bytes());
        o += 2;
        if o + s.len() > buf.len() {
            return Err("MQTT 配置超出容量");
        }
        buf[o..o + s.len()].copy_from_slice(s.as_bytes());
        o += s.len();
    }
    NorFlash::erase(flash, MQTT_SECTOR, MQTT_SECTOR + 4096).map_err(|_| "Flash 擦除失败")?;
    NorFlash::write(flash, MQTT_SECTOR, &buf[..o]).map_err(|_| "Flash 写入失败")
}

/// 读取 MQTT 配置 (无记录/损坏 → None)
pub fn load_mqtt(flash: &mut FlashStorage<'_>) -> Option<MqttSettings> {
    let mut buf = [0u8; 512];
    flash.read(MQTT_SECTOR, &mut buf).ok()?;
    if u32::from_le_bytes(buf[..4].try_into().ok()?) != MQTT_MAGIC {
        return None;
    }
    let mut o = 4usize;
    let mut out: [heapless::String<96>; 6] = core::array::from_fn(|_| heapless::String::new());
    for (i, cap) in MqttSettings::CAPS.iter().enumerate() {
        let len = u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?) as usize;
        o += 2;
        let s = core::str::from_utf8(buf.get(o..o + len)?).ok()?;
        o += len;
        if s.len() > *cap {
            return None;
        }
        *out.get_mut(i)? = heapless::String::try_from(s).ok()?;
    }
    Some(MqttSettings {
        endpoint: heapless::String::try_from(out[0].as_str()).ok()?,
        client_id: heapless::String::try_from(out[1].as_str()).ok()?,
        username: heapless::String::try_from(out[2].as_str()).ok()?,
        password: heapless::String::try_from(out[3].as_str()).ok()?,
        publish_topic: heapless::String::try_from(out[4].as_str()).ok()?,
        subscribe_topic: heapless::String::try_from(out[5].as_str()).ok()?,
    })
}

/// 启动自检: 写已知模式到备用扇区并读回, 验证存储层工作正常
pub fn self_test(flash: &mut FlashStorage<'_>) -> bool {
    const TEST_SECTOR: u32 = 0x9000; // NVS 分区首扇区 (未使用)
    let mut w = [0u8; 256];
    for (i, b) in w.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    if NorFlash::erase(flash, TEST_SECTOR, TEST_SECTOR + 4096).is_err() {
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
    if ok {
        defmt::info!("存储自检: 通过");
    } else {
        defmt::error!("存储自检: 读回数据不一致!");
    }
    ok
}
