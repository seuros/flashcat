use anyhow::{bail, Result};

use crate::fpga;
use crate::spi::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use crate::spi::SpiSpeed;
use crate::{prepare, VoltageChoice};

// Manufacturers that support READ UNIQUE ID (0x4B): 4 dummy bytes + 8-byte UID
const MFR_WINBOND:   u8 = 0xEF;
const MFR_GIGADEVICE: u8 = 0xC8;
const MFR_ISSI:      u8 = 0x9D;
const MFR_EON:       u8 = 0x1C;

pub async fn cmd_uid(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    let result = (async {
        let uid = match chip.mfr {
            MFR_WINBOND | MFR_GIGADEVICE | MFR_ISSI | MFR_EON => {
                read_uid_4b(&dev).await?
            }
            mfr => bail!(
                "READ UNIQUE ID not supported for manufacturer {mfr:#04x} ({})",
                chip.name
            ),
        };

        let hex: String = uid.iter().map(|b| format!("{b:02x}")).collect();
        println!("Chip: {}", chip.name);
        println!("UID:  {hex}");

        // Counterfeit heuristic: all-zero or all-FF UID is a red flag
        if uid.iter().all(|&b| b == 0x00) {
            eprintln!("\x1b[31m⚠ UID is all zeros — likely counterfeit\x1b[0m");
        } else if uid.iter().all(|&b| b == 0xFF) {
            eprintln!("\x1b[31m⚠ UID is all 0xFF — chip does not support unique ID\x1b[0m");
        }

        Ok(())
    }).await;
    fpga::vcc_off(&dev).await.ok();
    result
}

/// READ UNIQUE ID: opcode 0x4B + 4 dummy bytes + 8-byte UID (Winbond/GigaDevice/ISSI/Eon)
async fn read_uid_4b(dev: &crate::usb::UsbDevice) -> Result<Vec<u8>> {
    ss_enable(dev).await?;
    spibus_write(dev, &[0x4B, 0x00, 0x00, 0x00, 0x00]).await?;
    let uid = spibus_read(dev, 8).await?;
    ss_disable(dev).await?;
    Ok(uid)
}
