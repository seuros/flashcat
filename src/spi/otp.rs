use anyhow::{bail, Result};

use crate::spi::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use crate::units::MB_32;
use crate::usb::UsbDevice;

// Manufacturers whose security registers use the 0x48 register-select scheme
// (A15-12 = register number, A11-0 = byte offset). Other vendors (EON, ISSI,
// Macronix) gate OTP behind a separate "enter OTP mode" opcode and are not
// handled here.
const MFR_WINBOND: u8 = 0xEF;
const MFR_GIGADEVICE: u8 = 0xC8;

const CMD_READ_SECURITY_REG: u8 = 0x48;
const CMD_READ_SR2: u8 = 0x35;

/// Security-register layout for a supported chip.
#[derive(Debug, Clone, Copy)]
pub struct OtpGeometry {
    /// Number of security registers, numbered 1..=count.
    pub count: u8,
    /// Size of each register in bytes.
    pub size: u32,
}

/// Resolve security-register geometry from the manufacturer ID and chip size.
pub fn otp_geometry(mfr: u8, size_bytes: u32) -> Result<OtpGeometry> {
    match mfr {
        MFR_WINBOND => Ok(OtpGeometry { count: 3, size: 256 }),
        // GD25Q256-class parts carry 2048-byte registers; 128Mb and below use 1024.
        MFR_GIGADEVICE if size_bytes >= MB_32 => Ok(OtpGeometry { count: 3, size: 2048 }),
        MFR_GIGADEVICE => Ok(OtpGeometry { count: 3, size: 1024 }),
        _ => bail!(
            "READ SECURITY REGISTER (0x48) not supported for manufacturer {mfr:#04x}; \
             EON/ISSI/Macronix use a separate OTP-mode opcode"
        ),
    }
}

/// Base address of security register `reg` (1-based): A15-12 = reg, A11-0 = 0.
pub fn register_base(reg: u8) -> u32 {
    (reg as u32) << 12
}

/// READ SECURITY REGISTERS (0x48): opcode + address + 1 dummy byte + data.
/// `addr_bytes` is the chip's active address width (3 or 4); the chip is already
/// in the matching mode after detect.
pub async fn read_security_register(
    dev: &UsbDevice,
    reg: u8,
    addr_bytes: u8,
    size: u32,
) -> Result<Vec<u8>> {
    let base = register_base(reg);
    let mut cmd = vec![CMD_READ_SECURITY_REG];
    for shift in (0..addr_bytes).rev() {
        cmd.push((base >> (shift * 8)) as u8);
    }
    cmd.push(0x00); // dummy byte

    ss_enable(dev).await?;
    let wr = spibus_write(dev, &cmd).await;
    let rd = if wr.is_ok() {
        Some(spibus_read(dev, size as usize).await)
    } else {
        None
    };
    let dis = ss_disable(dev).await;

    wr?;
    let data = rd.expect("read attempted only when command write succeeded")?;
    dis?;

    if data.len() != size as usize {
        bail!("security register read returned {} bytes, expected {size}", data.len());
    }
    Ok(data)
}

/// OTP lock bits LB1-3 from Status Register-2 (bits 3-5). `true` = permanently locked.
pub async fn read_otp_locks(dev: &UsbDevice) -> Result<[bool; 3]> {
    ss_enable(dev).await?;
    let wr = spibus_write(dev, &[CMD_READ_SR2]).await;
    let rd = if wr.is_ok() {
        Some(spibus_read(dev, 1).await)
    } else {
        None
    };
    let dis = ss_disable(dev).await;

    wr?;
    let sr2 = rd.expect("read attempted only when command write succeeded")?;
    dis?;

    let sr2 = *sr2.first().unwrap_or(&0);
    Ok([
        sr2 & (1 << 3) != 0,
        sr2 & (1 << 4) != 0,
        sr2 & (1 << 5) != 0,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_matches_known_parts() {
        assert_eq!(otp_geometry(MFR_WINBOND, MB_32).unwrap().size, 256);
        assert_eq!(otp_geometry(MFR_GIGADEVICE, MB_32).unwrap().size, 2048);
        assert_eq!(otp_geometry(MFR_GIGADEVICE, MB_32 - 1).unwrap().size, 1024);
        assert!(otp_geometry(0x1C, MB_32).is_err());
    }

    #[test]
    fn register_bases_follow_a15_12_scheme() {
        assert_eq!(register_base(1), 0x1000);
        assert_eq!(register_base(2), 0x2000);
        assert_eq!(register_base(3), 0x3000);
    }
}
