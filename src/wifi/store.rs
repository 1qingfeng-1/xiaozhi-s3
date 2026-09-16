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
    let mut buf = [0xFFu8; 4096];
    buf[0] = volume.min(100);
    let _ = NorFlash::erase(flash, VOLUME_SECTOR, VOLUME_SECTOR + 4096);
    let _ = NorFlash::write(flash, VOLUME_SECTOR, &buf);
}

/// 读取音量 (无记录/擦除态返回默认 60)
pub fn load_volume(flash: &mut FlashStorage<'_>) -> u8 {
    let mut buf = [0u8; 1];
    match flash.read(VOLUME_SECTOR, &mut buf) {
        Ok(()) if buf[0] != 0xFF => buf[0].min(100),
        _ => 60,
    }
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
