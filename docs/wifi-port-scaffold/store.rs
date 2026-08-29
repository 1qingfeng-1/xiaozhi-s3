//! 配网凭据持久化 — 写入 Flash NVS 分区的最后一个 4KB 扇区
//!
//! 布局: magic u32 | ssid_len u16 | ssid | pass_len u16 | pass |
//! server_len u16 | server。写入前整扇区擦除。

use embedded_storage::nor_flash::NorFlash;
use esp_storage::FlashStorage;

use super::WifiConfig;

/// NVS 分区 (0x9000..0xF000) 的最后一个扇区
const SECTOR: u32 = 0xE000;
const SECTOR_SIZE: usize = 4096;
const MAGIC: u32 = 0x5849_4F5A; // 'ZOIX' 小端

/// 保存配置 (整扇区擦写)
pub fn save(flash: &mut FlashStorage, config: &WifiConfig) -> Result<(), &'static str> {
    let mut buf = [0u8; SECTOR_SIZE];
    buf[..4].copy_from_slice(&MAGIC.to_le_bytes());

    let mut o = 4usize;
    for part in [&config.ssid, &config.password, &config.server] {
        let bytes = part.as_bytes();
        let len = u16::try_from(bytes.len()).map_err(|_| "字段过长")?;
        buf[o..o + 2].copy_from_slice(&len.to_le_bytes());
        o += 2;
        if o + bytes.len() > SECTOR_SIZE {
            return Err("配置超出存储容量");
        }
        buf[o..o + bytes.len()].copy_from_slice(bytes);
        o += bytes.len();
    }

    flash
        .erase(SECTOR, SECTOR + SECTOR_SIZE as u32)
        .map_err(|_| "Flash 擦除失败")?;
    flash
        .write(SECTOR, &buf)
        .map_err(|_| "Flash 写入失败")
}

/// 读取配置 (无有效记录返回 None)
pub fn load(flash: &mut FlashStorage) -> Option<WifiConfig> {
    let mut buf = [0u8; SECTOR_SIZE];
    flash.read(SECTOR, &mut buf).ok()?;

    let magic = u32::from_le_bytes(buf[..4].try_into().ok()?);
    if magic != MAGIC {
        return None;
    }

    let mut o = 4usize;
    let mut parts = [heapless::String::<64>::new(); 3];
    for part in &mut parts {
        let len = u16::from_le_bytes(buf[o..o + 2].try_into().ok()?) as usize;
        o += 2;
        if o + len > SECTOR_SIZE {
            return None;
        }
        *part = heapless::String::try_from(core::str::from_utf8(&buf[o..o + len]).ok()?).ok()?;
        o += len;
    }

    Some(WifiConfig {
        ssid: heapless::String::try_from(&parts[0]).ok()?,
        password: heapless::String::try_from(&parts[1]).ok()?,
        server: heapless::String::try_from(&parts[2]).ok()?,
    })
}

/// 清除配置 (连接失败时调用, 回到配网模式)
pub fn clear(flash: &mut FlashStorage) -> Result<(), &'static str> {
    flash
        .erase(SECTOR, SECTOR + SECTOR_SIZE as u32)
        .map_err(|_| "Flash 擦除失败")
}
