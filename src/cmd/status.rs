use anyhow::{Result, bail};

use crate::spi::SpiSpeed;
use crate::spi::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use crate::units::{MB_16, MB_32};
use crate::usb::UsbDevice;
use crate::{VoltageChoice, prepare, with_cleanup};

const MFR_WINBOND: u8 = 0xEF;
const MFR_GIGADEVICE: u8 = 0xC8;
const MFR_MACRONIX: u8 = 0xC2;
const MFR_EON: u8 = 0x1C;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusDecoder {
    W25q4BitBp,
    W25q3BitBp,
    Gd25q256d,
    Mx25lF { has_4byte_bit: bool },
    Mx25lLegacy,
    En25qh,
}

impl StatusDecoder {
    async fn print(self, dev: &UsbDevice) -> Result<()> {
        match self {
            Self::W25q4BitBp => {
                let (sr1, sr2, sr3) = read_sr123(dev).await?;
                print_w25q_4bit_bp(sr1, sr2, sr3);
            }
            Self::W25q3BitBp => {
                let (sr1, sr2, sr3) = read_sr123(dev).await?;
                print_w25q_3bit_bp(sr1, sr2, sr3);
            }
            Self::Gd25q256d => {
                let (sr1, sr2, sr3) = read_sr123(dev).await?;
                print_gd25q256d(sr1, sr2, sr3);
            }
            Self::Mx25lF { has_4byte_bit } => {
                // Macronix has SR (0x05) + CR (0x15), no SR2 at 0x35.
                let (sr, cr) = read_sr_cr(dev).await?;
                print_mx25l_f(sr, cr, has_4byte_bit);
            }
            Self::Mx25lLegacy => {
                let sr = read_sr_byte(dev, 0x05).await?;
                print_mx25l_legacy(sr);
            }
            Self::En25qh => {
                let sr = read_sr_byte(dev, 0x05).await?;
                print_en25qh(sr);
            }
        }
        Ok(())
    }
}

fn status_decoder(mfr: u8, size_bytes: u32, name: &str) -> Option<StatusDecoder> {
    match (mfr, size_bytes, name) {
        (MFR_WINBOND, sz, _) if sz >= MB_32 => Some(StatusDecoder::W25q4BitBp),
        (MFR_WINBOND, _, _) => Some(StatusDecoder::W25q3BitBp),
        (MFR_GIGADEVICE, sz, name) if sz >= MB_32 && name.starts_with("GD25Q256D") => {
            Some(StatusDecoder::Gd25q256d)
        }
        (MFR_MACRONIX, sz, _) if sz >= MB_16 => Some(StatusDecoder::Mx25lF {
            has_4byte_bit: sz >= MB_32,
        }),
        (MFR_MACRONIX, _, _) => Some(StatusDecoder::Mx25lLegacy),
        (MFR_EON, _, _) => Some(StatusDecoder::En25qh),
        _ => None,
    }
}

async fn read_sr_byte(dev: &UsbDevice, opcode: u8) -> Result<u8> {
    ss_enable(dev).await?;
    let wr = spibus_write(dev, &[opcode]).await;
    let rd = spibus_read(dev, 1).await;
    let dis = ss_disable(dev).await;
    wr?;
    let data = rd?;
    dis?;
    data.first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("read SR {opcode:#04x}: empty response"))
}

pub async fn cmd_status(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    with_cleanup(&dev, async {
        println!("Chip: {} ({} MB)", chip.name, chip.size_bytes / (1024 * 1024));

        let Some(decoder) = status_decoder(chip.mfr, chip.size_bytes, &chip.name) else {
            bail!(
                "status decode for {} ({:#04x}) not implemented yet — supported: W25Q*, GD25Q256D, MX25L128/256 F-grade, legacy MX25L*, EN25QH*",
                chip.name, chip.mfr
            );
        };
        decoder.print(&dev).await?;

        Ok(())
    })
    .await
}

async fn read_sr123(dev: &UsbDevice) -> Result<(u8, u8, u8)> {
    Ok((
        read_sr_byte(dev, 0x05).await?,
        read_sr_byte(dev, 0x35).await?,
        read_sr_byte(dev, 0x15).await?,
    ))
}

async fn read_sr_cr(dev: &UsbDevice) -> Result<(u8, u8)> {
    Ok((
        read_sr_byte(dev, 0x05).await?,
        read_sr_byte(dev, 0x15).await?,
    ))
}

/// W25Q256JV / W25Q512JV layout: 4-bit BP, TB at SR1[6], no SEC; SR3 has ADS/ADP.
/// Reference: docs/datasheets/W25Q256JV-RevG.pdf §7.1, Figures 4a/4b/4c.
fn print_w25q_4bit_bp(sr1: u8, sr2: u8, sr3: u8) {
    let busy = bit(sr1, 0);
    let wel = bit(sr1, 1);
    let bp = (sr1 >> 2) & 0x0F;
    let tb = bit(sr1, 6);
    let srp0 = bit(sr1, 7);

    println!("\nSR1 (0x05) = {:#04x}  {}", sr1, bits(sr1));
    println!(
        "  BUSY    [0] = {}   {}",
        busy,
        if busy == 0 {
            "idle"
        } else {
            "program/erase in progress"
        }
    );
    println!(
        "  WEL     [1] = {}   {}",
        wel,
        if wel == 0 {
            "write-disabled"
        } else {
            "write-enabled"
        }
    );
    println!("  BP[3:0] [5:2] = {:04b}  {}", bp, bp_summary(bp));
    println!(
        "  TB      [6] = {}   protect from {}",
        tb,
        if tb == 0 { "TOP" } else { "BOTTOM" }
    );
    println!(
        "  SRP0    [7] = {}   {}",
        srp0,
        if srp0 == 0 {
            "status reg writable (software protect)"
        } else {
            "status reg locked (hardware/WP# protect)"
        }
    );

    let srl = bit(sr2, 0);
    let qe = bit(sr2, 1);
    let lb1 = bit(sr2, 3);
    let lb2 = bit(sr2, 4);
    let lb3 = bit(sr2, 5);
    let cmp = bit(sr2, 6);
    let sus = bit(sr2, 7);

    println!("\nSR2 (0x35) = {:#04x}  {}", sr2, bits(sr2));
    println!(
        "  SRL     [0] = {}   {}",
        srl,
        if srl == 0 {
            "not power-cycle locked"
        } else {
            "POWER-CYCLE LOCKED (SRP1)"
        }
    );
    println!(
        "  QE      [1] = {}   quad mode {}",
        qe,
        if qe == 0 { "disabled" } else { "ENABLED" }
    );
    println!(
        "  LB1     [3] = {}   security reg 1 {}",
        lb1,
        if lb1 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  LB2     [4] = {}   security reg 2 {}",
        lb2,
        if lb2 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  LB3     [5] = {}   security reg 3 {}",
        lb3,
        if lb3 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  CMP     [6] = {}   protection {}",
        cmp,
        if cmp == 0 {
            "normal"
        } else {
            "INVERTED (complement)"
        }
    );
    println!(
        "  SUS     [7] = {}   {}",
        sus,
        if sus == 0 {
            "no operation suspended"
        } else {
            "ERASE/PROGRAM SUSPENDED"
        }
    );

    let ads = bit(sr3, 0);
    let adp = bit(sr3, 1);
    let wps = bit(sr3, 2);
    let drv = (sr3 >> 5) & 0x03;

    println!("\nSR3 (0x15) = {:#04x}  {}", sr3, bits(sr3));
    println!(
        "  ADS     [0] = {}   currently in {}-byte address mode",
        ads,
        if ads == 0 { 3 } else { 4 }
    );
    println!(
        "  ADP     [1] = {}   powers up in {}-byte address mode",
        adp,
        if adp == 0 { 3 } else { 4 }
    );
    println!(
        "  WPS     [2] = {}   protection scheme: {}",
        wps,
        if wps == 0 {
            "BP/TB (status register)"
        } else {
            "individual block lock"
        }
    );
    println!(
        "  DRV[1:0] [6:5] = {:02b}  output driver strength: {}",
        drv,
        drv_strength(drv)
    );

    if srp0 == 1 && srl == 1 {
        println!(
            "\n  \x1b[31m! SRP0=1 and SRL=1: status register permanently locked (OTP). \x1b[0m"
        );
    } else if srl == 1 {
        println!("\n  \x1b[33m! SRL=1: status register locked until power cycle. \x1b[0m");
    } else if srp0 == 1 {
        println!("\n  \x1b[33m! SRP0=1: status register write depends on WP# pin level. \x1b[0m");
    }
    if cmp == 1 {
        println!(
            "  \x1b[33m! CMP=1: protected region is the COMPLEMENT of the BP-defined area. \x1b[0m"
        );
    }
    if sus == 1 {
        println!(
            "  \x1b[33m! SUS=1: an erase/program is suspended. Issue Resume (0x7A) or power cycle. \x1b[0m"
        );
    }
}

/// GD25Q256D (Rev 1.5) layout. Reference: docs/datasheets/GD25Q256D.pdf §6.1, Tables 7/8/9.
/// SR1 matches W25Q256JV (4-bit BP + TB + SRP0).
/// SR2 differs: ADS at bit 0, SUS2 at bit 2, SRP1 at bit 6, SUS1 at bit 7, no CMP.
/// SR3 differs: LC0/LC1 (DTR latency), PE/EE error flags, ADP at bit 4, DRV at 5/6, HOLD/RST at 7. No WPS.
fn print_gd25q256d(sr1: u8, sr2: u8, sr3: u8) {
    let busy = bit(sr1, 0);
    let wel = bit(sr1, 1);
    let bp = (sr1 >> 2) & 0x0F;
    let tb = bit(sr1, 6);
    let srp0 = bit(sr1, 7);

    println!("\nSR1 (0x05) = {:#04x}  {}", sr1, bits(sr1));
    println!(
        "  BUSY    [0] = {}   {}",
        busy,
        if busy == 0 {
            "idle"
        } else {
            "program/erase in progress"
        }
    );
    println!(
        "  WEL     [1] = {}   {}",
        wel,
        if wel == 0 {
            "write-disabled"
        } else {
            "write-enabled"
        }
    );
    println!("  BP[3:0] [5:2] = {:04b}  {}", bp, bp_summary(bp));
    println!(
        "  TB      [6] = {}   protect from {}",
        tb,
        if tb == 0 { "TOP" } else { "BOTTOM" }
    );
    println!(
        "  SRP0    [7] = {}   {}",
        srp0,
        if srp0 == 0 {
            "status reg writable (software protect)"
        } else {
            "status reg locked (hardware/WP# protect)"
        }
    );

    let ads = bit(sr2, 0);
    let qe = bit(sr2, 1);
    let sus2 = bit(sr2, 2);
    let lb1 = bit(sr2, 3);
    let lb2 = bit(sr2, 4);
    let lb3 = bit(sr2, 5);
    let srp1 = bit(sr2, 6);
    let sus1 = bit(sr2, 7);

    println!("\nSR2 (0x35) = {:#04x}  {}", sr2, bits(sr2));
    println!(
        "  ADS     [0] = {}   currently in {}-byte address mode",
        ads,
        if ads == 0 { 3 } else { 4 }
    );
    println!(
        "  QE      [1] = {}   quad mode {}",
        qe,
        if qe == 0 { "disabled" } else { "ENABLED" }
    );
    println!(
        "  SUS2    [2] = {}   {}",
        sus2,
        if sus2 == 0 {
            "no program suspended"
        } else {
            "PROGRAM SUSPENDED"
        }
    );
    println!(
        "  LB1     [3] = {}   security reg 1 {}",
        lb1,
        if lb1 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  LB2     [4] = {}   security reg 2 {}",
        lb2,
        if lb2 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  LB3     [5] = {}   security reg 3 {}",
        lb3,
        if lb3 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  SRP1    [6] = {}   {}",
        srp1,
        if srp1 == 0 {
            "not power-cycle locked"
        } else {
            "POWER-CYCLE LOCKED"
        }
    );
    println!(
        "  SUS1    [7] = {}   {}",
        sus1,
        if sus1 == 0 {
            "no erase suspended"
        } else {
            "ERASE SUSPENDED"
        }
    );

    let lc = sr3 & 0x03;
    let pe = bit(sr3, 2);
    let ee = bit(sr3, 3);
    let adp = bit(sr3, 4);
    let drv = (sr3 >> 5) & 0x03;
    let hold_rst = bit(sr3, 7);

    println!("\nSR3 (0x15) = {:#04x}  {}", sr3, bits(sr3));
    println!(
        "  LC[1:0] [1:0] = {:02b}  DTR latency code (DTR-capable parts only)",
        lc
    );
    println!(
        "  PE      [2] = {}   {}",
        pe,
        if pe == 0 {
            "last program OK"
        } else {
            "PROGRAM ERROR"
        }
    );
    println!(
        "  EE      [3] = {}   {}",
        ee,
        if ee == 0 {
            "last erase OK"
        } else {
            "ERASE ERROR"
        }
    );
    println!(
        "  ADP     [4] = {}   powers up in {}-byte address mode",
        adp,
        if adp == 0 { 3 } else { 4 }
    );
    println!(
        "  DRV[1:0] [6:5] = {:02b}  output driver strength: {}",
        drv,
        drv_strength(drv)
    );
    println!(
        "  HOLD/RST [7] = {}   pin function: {}",
        hold_rst,
        if hold_rst == 0 { "HOLD#" } else { "RESET#" }
    );

    if srp0 == 1 && srp1 == 1 {
        println!(
            "\n  \x1b[31m! SRP0=1 and SRP1=1: status register permanently locked (OTP). \x1b[0m"
        );
    } else if srp1 == 1 {
        println!("\n  \x1b[33m! SRP1=1: status register locked until power cycle. \x1b[0m");
    } else if srp0 == 1 {
        println!("\n  \x1b[33m! SRP0=1: status register write depends on WP# pin level. \x1b[0m");
    }
    if pe == 1 {
        println!("  \x1b[31m! PE=1: last program operation failed. Clear with Reset. \x1b[0m");
    }
    if ee == 1 {
        println!("  \x1b[31m! EE=1: last erase operation failed. Clear with Reset. \x1b[0m");
    }
    if sus1 == 1 || sus2 == 1 {
        println!("  \x1b[33m! Suspend active. Issue Resume (0x7A) or power cycle. \x1b[0m");
    }
}

/// W25Q32JV / W25Q64JV / W25Q128JV layout: 3-bit BP, SEC at SR1[6], TB at SR1[5].
/// SR3 has WPS at bit 2 but no ADS/ADP (those exist only on ≥256Mb variants).
/// Reference: docs/datasheets/W25Q128JV-RevH.pdf §7.1, Figures 4a/4b/4c.
fn print_w25q_3bit_bp(sr1: u8, sr2: u8, sr3: u8) {
    let busy = bit(sr1, 0);
    let wel = bit(sr1, 1);
    let bp = (sr1 >> 2) & 0x07;
    let tb = bit(sr1, 5);
    let sec = bit(sr1, 6);
    let srp0 = bit(sr1, 7);

    println!("\nSR1 (0x05) = {:#04x}  {}", sr1, bits(sr1));
    println!(
        "  BUSY    [0] = {}   {}",
        busy,
        if busy == 0 {
            "idle"
        } else {
            "program/erase in progress"
        }
    );
    println!(
        "  WEL     [1] = {}   {}",
        wel,
        if wel == 0 {
            "write-disabled"
        } else {
            "write-enabled"
        }
    );
    println!(
        "  BP[2:0] [4:2] = {:03b}  {}",
        bp,
        if bp == 0 {
            "no block protect"
        } else {
            "block protect active"
        }
    );
    println!(
        "  TB      [5] = {}   protect from {}",
        tb,
        if tb == 0 { "TOP" } else { "BOTTOM" }
    );
    println!(
        "  SEC     [6] = {}   protect unit: {}",
        sec,
        if sec == 0 {
            "64KB blocks"
        } else {
            "4KB sectors"
        }
    );
    println!(
        "  SRP0    [7] = {}   {}",
        srp0,
        if srp0 == 0 {
            "status reg writable (software protect)"
        } else {
            "status reg locked (hardware/WP# protect)"
        }
    );

    let srl = bit(sr2, 0);
    let qe = bit(sr2, 1);
    let lb1 = bit(sr2, 3);
    let lb2 = bit(sr2, 4);
    let lb3 = bit(sr2, 5);
    let cmp = bit(sr2, 6);
    let sus = bit(sr2, 7);

    println!("\nSR2 (0x35) = {:#04x}  {}", sr2, bits(sr2));
    println!(
        "  SRL     [0] = {}   {}",
        srl,
        if srl == 0 {
            "not power-cycle locked"
        } else {
            "POWER-CYCLE LOCKED (SRP1)"
        }
    );
    println!(
        "  QE      [1] = {}   quad mode {}",
        qe,
        if qe == 0 { "disabled" } else { "ENABLED" }
    );
    println!(
        "  LB1     [3] = {}   security reg 1 {}",
        lb1,
        if lb1 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  LB2     [4] = {}   security reg 2 {}",
        lb2,
        if lb2 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  LB3     [5] = {}   security reg 3 {}",
        lb3,
        if lb3 == 0 { "unlocked" } else { "OTP-LOCKED" }
    );
    println!(
        "  CMP     [6] = {}   protection {}",
        cmp,
        if cmp == 0 {
            "normal"
        } else {
            "INVERTED (complement)"
        }
    );
    println!(
        "  SUS     [7] = {}   {}",
        sus,
        if sus == 0 {
            "no operation suspended"
        } else {
            "ERASE/PROGRAM SUSPENDED"
        }
    );

    let wps = bit(sr3, 2);
    let drv = (sr3 >> 5) & 0x03;
    let hold_rst = bit(sr3, 7);

    println!("\nSR3 (0x15) = {:#04x}  {}", sr3, bits(sr3));
    println!(
        "  WPS     [2] = {}   protection scheme: {}",
        wps,
        if wps == 0 {
            "BP/TB/SEC (status register)"
        } else {
            "individual block lock"
        }
    );
    println!(
        "  DRV[1:0] [6:5] = {:02b}  output driver strength: {}",
        drv,
        drv_strength(drv)
    );
    println!(
        "  HOLD/RST [7] = {}   pin function: {}",
        hold_rst,
        if hold_rst == 0 { "HOLD#" } else { "RESET#" }
    );

    if srp0 == 1 && srl == 1 {
        println!(
            "\n  \x1b[31m! SRP0=1 and SRL=1: status register permanently locked (OTP). \x1b[0m"
        );
    } else if srl == 1 {
        println!("\n  \x1b[33m! SRL=1: status register locked until power cycle. \x1b[0m");
    } else if srp0 == 1 {
        println!("\n  \x1b[33m! SRP0=1: status register write depends on WP# pin level. \x1b[0m");
    }
    if cmp == 1 {
        println!(
            "  \x1b[33m! CMP=1: protected region is the COMPLEMENT of the BP-defined area. \x1b[0m"
        );
    }
    if sus == 1 {
        println!(
            "  \x1b[33m! SUS=1: an erase/program is suspended. Issue Resume (0x7A) or power cycle. \x1b[0m"
        );
    }
}

/// EN25QH128 (Rev G) — EON single-status-register layout.
/// Reference: docs/datasheets/EN25QH128-RevG.pdf Table 6.
/// SR (0x05): WIP[0], WEL[1], BP[3:0]@[5:2], WHDIS[6], SRP/OTP_LOCK[7].
/// **No SR2, no CR.** SR[6] disables WP#/HOLD# pin functions (was called "QE" in pre-G revisions).
fn print_en25qh(sr: u8) {
    let wip = bit(sr, 0);
    let wel = bit(sr, 1);
    let bp = (sr >> 2) & 0x0F;
    let whdis = bit(sr, 6);
    let srp = bit(sr, 7);

    println!("\nSR  (0x05) = {:#04x}  {}", sr, bits(sr));
    println!(
        "  WIP     [0] = {}   {}",
        wip,
        if wip == 0 {
            "idle"
        } else {
            "program/erase in progress"
        }
    );
    println!(
        "  WEL     [1] = {}   {}",
        wel,
        if wel == 0 {
            "write-disabled"
        } else {
            "write-enabled"
        }
    );
    println!("  BP[3:0] [5:2] = {:04b}  {}", bp, bp_summary(bp));
    println!(
        "  WHDIS   [6] = {}   WP# and HOLD# pin functions {}",
        whdis,
        if whdis == 0 {
            "ENABLED (default)"
        } else {
            "DISABLED (pins free for IO2/IO3)"
        }
    );
    println!(
        "  SRP     [7] = {}   {}",
        srp,
        if srp == 0 {
            "status reg writable"
        } else {
            "status reg locked (with WP# pin) / OTP_LOCK in OTP mode"
        }
    );

    if srp == 1 && whdis == 0 {
        println!(
            "\n  \x1b[33m! SRP=1 with WHDIS=0: status reg is hardware-protected when WP# is low. \x1b[0m"
        );
    }
    if whdis == 1 {
        println!(
            "\n  \x1b[33m! WHDIS=1: WP# and HOLD# pins disabled. Required for quad-mode use of pins 3/7 on 8-pin parts. \x1b[0m"
        );
    }
}

/// MX25L6406E-class (legacy Macronix, 64Mb and smaller E-grade).
/// Reference: docs/datasheets/MX25L6406E-v1.9.pdf §10-3.
/// Single SR (0x05) only — no CR, no QE bit (SR bit 6 hard-wired 0), no TB bit.
fn print_mx25l_legacy(sr: u8) {
    let wip = bit(sr, 0);
    let wel = bit(sr, 1);
    let bp = (sr >> 2) & 0x0F;
    let bit6 = bit(sr, 6);
    let srwd = bit(sr, 7);

    println!("\nSR  (0x05) = {:#04x}  {}", sr, bits(sr));
    println!(
        "  WIP     [0] = {}   {}",
        wip,
        if wip == 0 {
            "idle"
        } else {
            "program/erase in progress"
        }
    );
    println!(
        "  WEL     [1] = {}   {}",
        wel,
        if wel == 0 {
            "write-disabled"
        } else {
            "write-enabled"
        }
    );
    println!("  BP[3:0] [5:2] = {:04b}  {}", bp, bp_summary(bp));
    println!(
        "  (reserved) [6] = {}   hard-wired 0 (no QE bit on this generation)",
        bit6
    );
    println!(
        "  SRWD    [7] = {}   {}",
        srwd,
        if srwd == 0 {
            "status reg writable"
        } else {
            "status reg locked (with WP# pin)"
        }
    );

    if srwd == 1 {
        println!(
            "\n  \x1b[33m! SRWD=1: hardware-protected mode active when WP# pin is low. \x1b[0m"
        );
    }
    if bit6 == 1 {
        println!(
            "\n  \x1b[33m! SR[6]=1 on a part that should hard-wire it to 0 — may be a newer F-grade with QE bit. \x1b[0m"
        );
    }
}

/// MX25L12835F / MX25L25635F — Macronix F-grade, 3V.
/// References: docs/datasheets/MX25L12835F-v1.7.pdf and MX25L25635F-v1.5.pdf.
/// SR (0x05): WIP[0], WEL[1], BP[3:0]@[5:2], QE[6], SRWD[7]. **No SR2 at 0x35**.
/// CR (0x15): ODS[2:0]@[2:0], TB[3], reserved[4], bit 5 reserved on 128Mb
/// and 4BYTE on 256Mb, DC[1:0]@[7:6].
fn print_mx25l_f(sr: u8, cr: u8, has_4byte_bit: bool) {
    let wip = bit(sr, 0);
    let wel = bit(sr, 1);
    let bp = (sr >> 2) & 0x0F;
    let qe = bit(sr, 6);
    let srwd = bit(sr, 7);

    println!("\nSR  (0x05) = {:#04x}  {}", sr, bits(sr));
    println!(
        "  WIP     [0] = {}   {}",
        wip,
        if wip == 0 {
            "idle"
        } else {
            "program/erase in progress"
        }
    );
    println!(
        "  WEL     [1] = {}   {}",
        wel,
        if wel == 0 {
            "write-disabled"
        } else {
            "write-enabled"
        }
    );
    println!("  BP[3:0] [5:2] = {:04b}  {}", bp, bp_summary(bp));
    println!(
        "  QE      [6] = {}   quad mode {}",
        qe,
        if qe == 0 { "disabled" } else { "ENABLED" }
    );
    println!(
        "  SRWD    [7] = {}   {}",
        srwd,
        if srwd == 0 {
            "status reg writable"
        } else {
            "status reg locked (with WP# pin)"
        }
    );

    let ods = cr & 0x07;
    let tb = bit(cr, 3);
    let dc = (cr >> 6) & 0x03;

    println!("\nCR  (0x15) = {:#04x}  {}", cr, bits(cr));
    println!(
        "  ODS[2:0] [2:0] = {:03b}  output driver: {}",
        ods,
        mx_ods(ods)
    );
    println!(
        "  TB      [3] = {}   protect from {} (OTP)",
        tb,
        if tb == 0 { "TOP" } else { "BOTTOM" }
    );
    println!("  reserved [4] = {}", bit(cr, 4));
    if has_4byte_bit {
        let four_byte = bit(cr, 5);
        println!(
            "  4BYTE   [5] = {}   {}-byte address mode",
            four_byte,
            if four_byte == 0 { 3 } else { 4 }
        );
    } else {
        println!("  reserved [5] = {}", bit(cr, 5));
    }
    println!(
        "  DC[1:0] [7:6] = {:02b}  fast-read dummy cycles ({})",
        dc,
        mx_dc(dc)
    );

    if srwd == 1 {
        println!(
            "\n  \x1b[33m! SRWD=1: hardware-protected mode active when WP# pin is low. \x1b[0m"
        );
    }
    if qe == 1 {
        println!(
            "  \x1b[33m! QE=1: HOLD#/RESET# pin functions disabled; pins act as IO2/IO3. \x1b[0m"
        );
    }
}

fn mx_ods(ods: u8) -> &'static str {
    // MX25L25635F Output Driver Strength Table
    match ods {
        0b001 => "90 Ω",
        0b010 => "60 Ω",
        0b011 => "45 Ω",
        0b101 => "20 Ω",
        0b110 => "15 Ω",
        0b111 => "30 Ω (default)",
        _ => "reserved",
    }
}

fn mx_dc(dc: u8) -> &'static str {
    // MX25L25635F Dummy Cycle and Frequency Table — abbreviated
    match dc {
        0b00 => "8 (default, ≤104MHz)",
        0b01 => "6",
        0b10 => "8",
        0b11 => "10 (≤166MHz)",
        _ => "?",
    }
}

fn bit(reg: u8, n: u8) -> u8 {
    (reg >> n) & 1
}

fn bits(reg: u8) -> String {
    format!("{:04b} {:04b}", (reg >> 4) & 0x0F, reg & 0x0F)
}

fn bp_summary(bp: u8) -> &'static str {
    // W25Q256JV: BP[3:0] together with TB select the protected range.
    // BP=0000 → none; any nonzero BP protects some range.
    if bp == 0 {
        "no block protect"
    } else {
        "block protect active (see datasheet table for exact range)"
    }
}

fn drv_strength(drv: u8) -> &'static str {
    match drv {
        0b00 => "100% (default)",
        0b01 => "75%",
        0b10 => "50%",
        0b11 => "25%",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::MB_8;

    #[test]
    fn routes_mx25l128_to_f_grade_decoder() {
        assert_eq!(
            status_decoder(MFR_MACRONIX, MB_16, "MX25L128"),
            Some(StatusDecoder::Mx25lF {
                has_4byte_bit: false
            })
        );
    }

    #[test]
    fn routes_mx25l256_to_f_grade_decoder_with_4byte_bit() {
        assert_eq!(
            status_decoder(MFR_MACRONIX, MB_32, "MX25L256"),
            Some(StatusDecoder::Mx25lF {
                has_4byte_bit: true
            })
        );
    }

    #[test]
    fn routes_small_mx25l_parts_to_legacy_decoder() {
        assert_eq!(
            status_decoder(MFR_MACRONIX, MB_8, "MX25L64"),
            Some(StatusDecoder::Mx25lLegacy)
        );
    }
}
