use anyhow::{bail, Result};
use std::time::Duration;
use tracing::debug;

use crate::chip::ResolvedChip;
use crate::usb::UsbDevice;

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};

const MFR_WINBOND:    u8 = 0xEF;
const MFR_GIGADEVICE: u8 = 0xC8;

pub struct WpStatus {
    pub sr1: u8,
    pub sr2: Option<u8>,
    pub sr3: Option<u8>,
    /// Human-readable protected address range.
    pub range: String,
    /// How the protection lock is enforced.
    pub mode: &'static str,
    /// SR3 WPS=1: per-block lock mode active instead of BP-range mode.
    pub block_lock_mode: bool,
}

async fn read_sr(dev: &UsbDevice, opcode: u8) -> Result<u8> {
    ss_enable(dev).await?;
    let wr   = spibus_write(dev, &[opcode]).await;
    let rd   = spibus_read(dev, 1).await;
    let dis  = ss_disable(dev).await;
    wr?;
    let data = rd?;
    dis?;
    if data.is_empty() {
        bail!("RDSR short response: 0 bytes");
    }
    Ok(data[0])
}

/// Read write-protection status registers.
/// SR2 (0x35) and SR3 (0x15) are only read for Winbond / GigaDevice parts.
pub async fn read_wp_status(dev: &UsbDevice, chip: &ResolvedChip) -> Result<WpStatus> {
    let sr1 = read_sr(dev, 0x05).await?;

    let (sr2, sr3) = match chip.mfr {
        MFR_WINBOND | MFR_GIGADEVICE => {
            let sr2 = read_sr(dev, 0x35).await?;
            let sr3 = read_sr(dev, 0x15).await?;
            (Some(sr2), Some(sr3))
        }
        _ => (None, None),
    };

    let bp   = (sr1 >> 2) & 0x07;
    let tb   = (sr1 >> 5) & 0x01;
    let sec  = (sr1 >> 6) & 0x01;
    let srp0 = (sr1 >> 7) & 0x01;

    let (cmp, srp1, block_lock_mode) = match (sr2, sr3) {
        (Some(s2), Some(s3)) => {
            let cmp  = (s2 >> 6) & 0x01;
            let srp1 = (s2 >> 7) & 0x01;
            let wps  = (s3 >> 2) & 0x01;
            (cmp, srp1, wps == 1)
        }
        _ => (0, 0, false),
    };

    let range = decode_range_spi25(chip.size_bytes, bp, tb, sec, cmp);

    let mode = match (srp0, srp1) {
        (0, 0) => "software",
        (1, 0) => "hardware (WP# pin)",
        (0, 1) => "power-cycle",
        (1, 1) => "permanent (OTP)",
        _      => "unknown",
    };

    Ok(WpStatus { sr1, sr2, sr3, range, mode, block_lock_mode })
}

/// Decode the protected range from BP/TB/SEC/CMP bits (DECODE_RANGE_SPI25 style).
///
/// - BP=0 and CMP=0 → no protection
/// - BP=7 and CMP=0 → entire chip
/// - Otherwise: 2^(bp-1) × base_unit bytes from top (TB=0) or bottom (TB=1)
/// - CMP=1 inverts: the *complement* of the base range is protected
fn decode_range_spi25(size: u32, bp: u8, tb: u8, sec: u8, cmp: u8) -> String {
    if bp == 0 && cmp == 0 {
        return "none".to_string();
    }

    // base unit: 64 KB normally, 4 KB when SEC=1
    let base = if sec == 1 { 4 * 1024u32 } else { 64 * 1024u32 };
    let protected = if bp == 0 {
        0u32
    } else {
        (1u32 << (bp - 1)).saturating_mul(base).min(size)
    };

    let base_desc = if protected == 0 {
        "none".to_string()
    } else if protected >= size {
        format!("all ({} KB)", size / 1024)
    } else {
        let side = if tb == 0 { "upper" } else { "lower" };
        format!("{} {} KB", side, protected / 1024)
    };

    if cmp == 1 {
        // CMP inverts: the complement of base_desc is protected
        format!("complement of {base_desc}")
    } else {
        base_desc
    }
}

/// Write-enable then write a single status register byte via the given opcode.
/// Polls WIP until the SR write completes (typically < 15 ms).
async fn write_sr(dev: &UsbDevice, wren_opcode: u8, wrsr_opcode: u8, value: u8) -> Result<()> {
    write_sr_bytes(dev, wren_opcode, wrsr_opcode, &[value]).await
}

/// Write-enable then write one or two status register bytes atomically via the
/// given opcode.  Used for 2-byte WRSR(0x01, sr1, sr2) on Winbond/GigaDevice
/// parts where a single-byte WRSR resets SR2 to defaults (clearing QE etc.).
/// Polls WIP until the SR write completes (typically < 15 ms).
async fn write_sr_bytes(
    dev: &UsbDevice,
    wren_opcode: u8,
    wrsr_opcode: u8,
    values: &[u8],
) -> Result<()> {
    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[wren_opcode]).await;
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    let mut payload = Vec::with_capacity(1 + values.len());
    payload.push(wrsr_opcode);
    payload.extend_from_slice(values);

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &payload).await;
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    // Poll WIP: SR writes complete in < 15 ms; allow up to 500 ms.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let sr1 = read_sr(dev, 0x05).await?;
        if sr1 & 0x01 == 0 {
            debug!("SR write complete (SR1={sr1:#04x})");
            return Ok(());
        }
    }
    bail!("timeout waiting for SR write to complete");
}

/// Protect the entire chip: sets BP[2:0]=111, clears TB/SEC/CMP.
///
/// For Winbond/GigaDevice: writes SR1 via 0x01, clears CMP in SR2 via 0x31.
/// For all other vendors: writes SR1 via 0x01.
///
/// Uses non-volatile WREN (0x06) so protection survives power cycles.
pub async fn protect_chip(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    let sr1 = read_sr(dev, 0x05).await?;

    // Check SRP — if hardware-locked, writes will be silently ignored.
    if sr1 & 0x80 != 0 {
        bail!("SRP0 is set — status register is hardware-write-protected (WP# pin or permanent)");
    }

    // Pick the right BP layout based on manufacturer and chip size:
    //   Winbond W25Q256+ (MFR=0xEF, >16MB): 4-bit BP layout — BP3(6)|BP2(5)|BP1(4)|BP0(3)|TB(2), set_mask=0x78
    //   All others (Macronix, EON, GigaDevice, ISSI, etc.): 3-bit BP layout — bit 6 is QE/SEC, set_mask=0x1C
    // preserve_mask 0x83 = 1000_0011: keeps SRP0(7) and WIP/WEL(1:0), clears SEC/BP3 at bit 6.
    let is_winbond_4bit_bp = chip.mfr == MFR_WINBOND && chip.size_bytes > 16 * 1024 * 1024;
    let (preserve_mask, set_mask, verify_mask) = if is_winbond_4bit_bp {
        // W25Q256+: 4-bit BP layout, BP3 at bit 6
        (0x83u8, 0x78u8, 0x78u8)
    } else {
        // 3-bit BP layout, bit 6 is QE or SEC — must not be touched
        (0x83u8, 0x1Cu8, 0x1Cu8)
    };
    let new_sr1 = (sr1 & preserve_mask) | set_mask;

    // For Winbond/GD: read SR2 first and include it in a single 2-byte WRSR(0x01)
    // so that SR2 (including QE bit) is preserved atomically.  A single-byte
    // WRSR on older BV/DV/FV revisions resets SR2 to defaults, silently clearing
    // QE.  We also clear CMP (SR2 bit 6) in the same transaction to avoid
    // complement-inversion of the protected range.
    if matches!(chip.mfr, MFR_WINBOND | MFR_GIGADEVICE) {
        let sr2 = read_sr(dev, 0x35).await?;
        let new_sr2 = sr2 & !0x40u8; // clear CMP
        debug!("protect: SR1 {sr1:#04x} → {new_sr1:#04x}, SR2 {sr2:#04x} → {new_sr2:#04x}");
        write_sr_bytes(dev, 0x06, 0x01, &[new_sr1, new_sr2]).await?;
    } else {
        debug!("protect: SR1 {sr1:#04x} → {new_sr1:#04x}");
        write_sr(dev, 0x06, 0x01, new_sr1).await?;
    }

    // Verify BP bits were actually written (SRP could have blocked it silently).
    let readback = read_sr(dev, 0x05).await?;
    if readback & verify_mask != verify_mask {
        bail!("protection write was rejected by chip (readback SR1={readback:#04x}) — check WP# pin");
    }

    Ok(())
}

/// Unprotect the entire chip: clears BP[2:0], TB, SEC, and CMP.
///
/// For Winbond/GigaDevice: also clears CMP in SR2 via 0x31.
/// Uses non-volatile WREN (0x06).
pub async fn unprotect_chip(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    let sr1 = read_sr(dev, 0x05).await?;

    if sr1 & 0x80 != 0 {
        bail!("SRP0 is set — status register is hardware-write-protected (WP# pin or permanent)");
    }

    // Clear BP[2:0](4:2), TB(5), SEC(6); preserve SRP0(7), WIP(0), WEL(1).
    let new_sr1 = sr1 & 0x83;

    // For Winbond/GD: read SR2 first and include it in a single 2-byte WRSR(0x01)
    // to preserve SR2 atomically (prevents older BV/DV/FV revisions from resetting
    // SR2 to defaults and silently clearing QE).  Clear CMP (SR2 bit 6) in the
    // same transaction — CMP=1 with BP=0 would mean the entire chip is protected.
    if matches!(chip.mfr, MFR_WINBOND | MFR_GIGADEVICE) {
        let sr2 = read_sr(dev, 0x35).await?;
        let new_sr2 = sr2 & !0x40u8; // clear CMP
        debug!("unprotect: SR1 {sr1:#04x} → {new_sr1:#04x}, SR2 {sr2:#04x} → {new_sr2:#04x}");
        write_sr_bytes(dev, 0x06, 0x01, &[new_sr1, new_sr2]).await?;
    } else {
        debug!("unprotect: SR1 {sr1:#04x} → {new_sr1:#04x}");
        write_sr(dev, 0x06, 0x01, new_sr1).await?;
    }

    // Verify — BP bits (and SEC/TB) should all be zero now.
    // Winbond W25Q256+ (4-bit BP): check BP3(6)|BP2(5)|BP1(4)|BP0(3), mask 0x78.
    // All others (3-bit BP): check SEC(6)|TB(5)|BP2(4)|BP1(3)|BP0(2), mask 0x7C.
    let is_winbond_4bit_bp = chip.mfr == MFR_WINBOND && chip.size_bytes > 16 * 1024 * 1024;
    let check_mask = if is_winbond_4bit_bp { 0x78u8 } else { 0x7Cu8 };
    let readback = read_sr(dev, 0x05).await?;
    if readback & check_mask != 0 {
        bail!("unprotect write was rejected by chip (readback SR1={readback:#04x}) — check WP# pin");
    }

    Ok(())
}

impl WpStatus {
    /// One-line summary suitable for display in `detect` output.
    pub fn summary(&self) -> String {
        let mut s = format!("{} ({})", self.range, self.mode);
        if self.block_lock_mode {
            s.push_str(" [block-lock mode]");
        }
        s.push_str(&format!("  [SR1={:#04x}", self.sr1));
        if let Some(sr2) = self.sr2 {
            s.push_str(&format!(" SR2={sr2:#04x}"));
        }
        if let Some(sr3) = self.sr3 {
            s.push_str(&format!(" SR3={sr3:#04x}"));
        }
        s.push(']');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::decode_range_spi25;

    const SIZE_4MB: u32 = 4 * 1024 * 1024;

    #[test]
    fn no_protection() {
        assert_eq!(decode_range_spi25(SIZE_4MB, 0, 0, 0, 0), "none");
    }

    #[test]
    fn all_protected() {
        assert_eq!(decode_range_spi25(SIZE_4MB, 7, 0, 0, 0), "all (4096 KB)");
    }

    #[test]
    fn upper_quarter_bp3() {
        // bp=3: 2^2 * 64KB = 256KB = 1/16 of 4MB... let's verify the math
        // 4MB: bp=3 → 4 * 64KB = 256KB
        assert_eq!(decode_range_spi25(SIZE_4MB, 3, 0, 0, 0), "upper 256 KB");
    }

    #[test]
    fn lower_half_tb1_bp6() {
        // bp=6: 2^5 * 64KB = 2MB, TB=1 → lower
        assert_eq!(decode_range_spi25(SIZE_4MB, 6, 1, 0, 0), "lower 2048 KB");
    }

    #[test]
    fn sec_mode_small_sector() {
        // bp=1, SEC=1: 2^0 * 4KB = 4KB upper
        assert_eq!(decode_range_spi25(SIZE_4MB, 1, 0, 1, 0), "upper 4 KB");
    }

    #[test]
    fn cmp_inverts_range() {
        let r = decode_range_spi25(SIZE_4MB, 3, 0, 0, 1);
        assert!(r.starts_with("complement of"));
    }

    #[test]
    fn cmp_with_bp0_is_complement_of_none() {
        // CMP=1, BP=0 → complement of "none" = effectively all protected
        let r = decode_range_spi25(SIZE_4MB, 0, 0, 0, 1);
        assert_eq!(r, "complement of none");
    }
}
