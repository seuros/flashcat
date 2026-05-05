use anyhow::{bail, Result};
use std::time::Duration;
use tracing::debug;

use crate::chip::ResolvedChip;
use crate::usb::UsbDevice;

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use super::write::write_enable;

const MFR_WINBOND: u8 = 0xEF;

fn require_winbond(chip: &ResolvedChip) -> Result<()> {
    if chip.mfr != MFR_WINBOND {
        bail!(
            "block lock is Winbond-specific (mfr={:#04x} {})",
            chip.mfr,
            chip.name
        );
    }
    Ok(())
}

/// Build the address byte sequence for a given chip (3 or 4 bytes, big-endian).
fn addr_vec(chip: &ResolvedChip, addr: u32) -> Vec<u8> {
    if chip.addr_bytes == 4 {
        vec![
            ((addr >> 24) & 0xFF) as u8,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]
    } else {
        vec![
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]
    }
}

/// Read the lock status of the block containing `addr`.
/// Returns true if the block is locked (bit 0 of the response byte is set).
pub async fn read_block_lock(dev: &UsbDevice, chip: &ResolvedChip, addr: u32) -> Result<bool> {
    require_winbond(chip)?;
    if addr >= chip.size_bytes {
        bail!("address {addr:#010x} is out of range for {} (size {:#010x})", chip.name, chip.size_bytes);
    }
    let mut cmd = vec![0x3D]; // READ_BLOCK_LOCK
    cmd.extend_from_slice(&addr_vec(chip, addr));
    ss_enable(dev).await?;
    let wr = spibus_write(dev, &cmd).await;
    let rd = spibus_read(dev, 1).await;
    let dis = ss_disable(dev).await;
    wr?;
    let data = rd?;
    dis?;
    if data.is_empty() {
        bail!("READ_BLOCK_LOCK short response: 0 bytes");
    }
    debug!("read_block_lock addr={addr:#010x} byte={:02x} locked={}", data[0], data[0] & 0x01 != 0);
    Ok(data[0] & 0x01 != 0)
}

/// Lock the block containing `addr` (volatile, requires WREN).
/// IND_BLOCK_LOCK (0x36) completes in < 1 µs — no WIP polling needed.
pub async fn lock_block(dev: &UsbDevice, chip: &ResolvedChip, addr: u32) -> Result<()> {
    require_winbond(chip)?;
    if addr >= chip.size_bytes {
        bail!("address {addr:#010x} is out of range for {} (size {:#010x})", chip.name, chip.size_bytes);
    }
    write_enable(dev).await?;
    let mut cmd = vec![0x36]; // IND_BLOCK_LOCK
    cmd.extend_from_slice(&addr_vec(chip, addr));
    ss_enable(dev).await?;
    let r = spibus_write(dev, &cmd).await;
    let d = ss_disable(dev).await;
    r?;
    d?;
    Ok(())
}

/// Unlock the block containing `addr` (volatile, requires WREN).
/// IND_BLOCK_UNLOCK (0x39) completes in < 1 µs — no WIP polling needed.
pub async fn unlock_block(dev: &UsbDevice, chip: &ResolvedChip, addr: u32) -> Result<()> {
    require_winbond(chip)?;
    if addr >= chip.size_bytes {
        bail!("address {addr:#010x} is out of range for {} (size {:#010x})", chip.name, chip.size_bytes);
    }
    write_enable(dev).await?;
    let mut cmd = vec![0x39]; // IND_BLOCK_UNLOCK
    cmd.extend_from_slice(&addr_vec(chip, addr));
    ss_enable(dev).await?;
    let r = spibus_write(dev, &cmd).await;
    let d = ss_disable(dev).await;
    r?;
    d?;
    Ok(())
}

/// Lock all blocks globally (volatile, ~200µs typical, requires WREN).
/// A 1 ms sleep is sufficient; WIP polling is not needed for this operation.
pub async fn global_lock(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    require_winbond(chip)?;
    write_enable(dev).await?;
    ss_enable(dev).await?;
    let r = spibus_write(dev, &[0x7E]).await; // GLOBAL_BLOCK_LOCK
    let d = ss_disable(dev).await;
    r?;
    d?;
    tokio::time::sleep(Duration::from_millis(1)).await;
    Ok(())
}

/// Unlock all blocks globally (volatile, ~200µs typical, requires WREN).
/// A 1 ms sleep is sufficient; WIP polling is not needed for this operation.
pub async fn global_unlock(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    require_winbond(chip)?;
    write_enable(dev).await?;
    ss_enable(dev).await?;
    let r = spibus_write(dev, &[0x98]).await; // GLOBAL_BLOCK_UNLOCK
    let d = ss_disable(dev).await;
    r?;
    d?;
    tokio::time::sleep(Duration::from_millis(1)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::addr_vec;
    use crate::chip::{ResolvedChip, ParamSource};
    use crate::db::ChipVoltage;

    fn make_chip(addr_bytes: u8) -> ResolvedChip {
        ResolvedChip {
            name: "TestChip".to_string(),
            mfr: 0xEF,
            id1: 0x40,
            id2: 0x18,
            voltage: ChipVoltage::V3_3,
            size_bytes: 16 * 1024 * 1024,
            page_size: 256,
            erase_size: 4096,
            erase_types: vec![],
            addr_bytes,
            quad: false,
            source: ParamSource::Database,
            chip_erase_max_ms: None,
        }
    }

    #[test]
    fn addr_vec_3byte() {
        let chip = make_chip(3);
        assert_eq!(addr_vec(&chip, 0x001234), vec![0x00, 0x12, 0x34]);
    }

    #[test]
    fn addr_vec_4byte() {
        let chip = make_chip(4);
        assert_eq!(addr_vec(&chip, 0x01001234), vec![0x01, 0x00, 0x12, 0x34]);
    }

    #[test]
    fn out_of_range_addr_is_larger_than_size() {
        // 16 MiB chip: valid range is 0x000000..=0xFF_FFFF; 0x100_0000 equals size_bytes — out of range.
        let chip = make_chip(3);
        assert_eq!(chip.size_bytes, 16 * 1024 * 1024);
        assert!(0x0100_0000_u32 >= chip.size_bytes, "0x0100_0000 must be >= size_bytes (i.e. out of range)");
        // The last valid address must be strictly less than size_bytes.
        assert!(0x00FF_FFFF_u32 < chip.size_bytes, "0x00FF_FFFF must be < size_bytes (i.e. in range)");
    }
}
