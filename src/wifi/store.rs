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
