use anyhow::Result;

use crate::chip::ParamSource;
use crate::fpga::Voltage;
use crate::spi::{SpiSpeed, read_wp_status};
use crate::{VoltageChoice, power_down_and_vcc_off, prepare, setup, with_cleanup};

pub async fn cmd_detect(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    match prepare(vc, speed).await {
        Ok((dev, chip, voltage)) => {
            with_cleanup(&dev, async {
                println!("Chip:      {}", chip.name);
                println!(
                    "Size:      {} MB ({} bytes)",
                    chip.size_bytes / 1024 / 1024,
                    chip.size_bytes
                );
                println!("Page:      {} bytes", chip.page_size);
                println!("Erase:     {} bytes", chip.erase_size);
                println!("Addr:      {}-byte", chip.addr_bytes);
                println!("Voltage:   {:?}", voltage);
                println!(
                    "SFDP:      {}",
                    match chip.source {
                        ParamSource::DatabaseWithSfdp => "yes",
                        ParamSource::DatabaseWithPartialSfdp => "partial (pre-JESD216A)",
                        ParamSource::Sfdp => "yes (no DB match)",
                        ParamSource::Database => "no",
                    }
                );
                match read_wp_status(&dev, &chip).await {
                    Ok(wp) => println!("WP:        {}", wp.summary()),
                    Err(e) => println!("WP:        unavailable ({e})"),
                }
                // No counterfeit warning here — many genuine pre-JESD216 chips
                // (e.g. early Macronix MX25L6406E, lots of older Winbond W25Q
                // parts) ship with no SFDP or only a density-only table. The
                // official fcusb app never queries SFDP at all and matches RDID
                // against its DB. We only warn loudly when SFDP *is* present
                // and disagrees with the DB (handled in merge_db_with_sfdp).
                Ok(())
            })
            .await
        }
        // NOTE: matches the literal string from anyhow::bail! in prepare() (main.rs).
        // This is a string-match sentinel; if the bail message ever changes this branch
        // will silently stop matching and the error will propagate as Err(e) below.
        Err(e) if e.to_string().contains("no chip detected") => {
            println!("No chip identified — running diagnostic probe:");
            diagnostic_probe(vc, speed).await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// When identification fails, do a minimal raw probe so the user can see what
/// the chip actually said. Explicit voltage choices are never escalated; auto
/// probes mirror the safe detect order (1.8V before 3.3V). Helps distinguish a
/// genuine-but-unknown chip from a non-response without over-volting 1.8V parts.
async fn diagnostic_probe(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    for voltage in diagnostic_voltages(vc).into_iter().flatten() {
        println!("\n  probe @ {voltage:?}:");
        let dev = match setup(voltage, speed).await {
            Ok(d) => d,
            Err(e) => {
                println!("    setup failed: {e}");
                continue;
            }
        };

        // Try once cold; if blank, software-reset and retry (matches detect path).
        let rdid = crate::spi::detect::rdid(&dev).await;
        let rdid = match rdid {
            Ok(id) if id[0] == 0xFF || id[0] == 0x00 => {
                let _ = crate::spi::detect::software_reset(&dev).await;
                crate::spi::detect::rdid(&dev).await
            }
            other => other,
        };

        match rdid {
            Ok([0xFF, 0xFF, 0xFF]) => println!(
                "    RDID = FF FF FF  (no response — chip not powered, mis-seated, or wrong voltage)"
            ),
            Ok([0x00, 0x00, 0x00]) => println!(
                "    RDID = 00 00 00  (no response — chip floating, missing pull-ups, or dead)"
            ),
            Ok([mfr, id1, id2]) => {
                println!(
                    "    RDID = {mfr:02X} {id1:02X} {id2:02X}  ({})",
                    vendor_name(mfr)
                );
                match crate::spi::sfdp::try_read_sfdp(&dev).await {
                    Some(info) => println!(
                        "    SFDP = present (rev {}.{}, size {} MB, page {} B)",
                        info.sfdp_rev.0,
                        info.sfdp_rev.1,
                        info.size_bytes / (1024 * 1024),
                        info.page_size,
                    ),
                    None => println!("    SFDP = absent or unreadable"),
                }
            }
            Err(e) => println!("    RDID failed: {e}"),
        }

        power_down_and_vcc_off(&dev).await;
    }
    Ok(())
}

fn diagnostic_voltages(vc: VoltageChoice) -> [Option<Voltage>; 2] {
    match vc {
        VoltageChoice::Auto => [Some(Voltage::V1_8), Some(Voltage::V3_3)],
        VoltageChoice::Explicit(voltage) => [Some(voltage), None],
    }
}

fn vendor_name(mfr: u8) -> &'static str {
    match mfr {
        0x01 => "Spansion/Cypress",
        0x1C => "EON",
        0x1F => "Atmel/Adesto",
        0x20 => "Micron/Numonyx",
        0x37 => "AMIC",
        0x52 => "ESMT",
        0x68 => "Boya",
        0x85 => "PUYA",
        0x9D => "ISSI",
        0xBF => "SST/Microchip",
        0xC2 => "Macronix",
        0xC8 => "GigaDevice",
        0xEF => "Winbond",
        _ => "unknown vendor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_diagnostic_voltage_never_escalates() {
        assert_eq!(
            diagnostic_voltages(VoltageChoice::Explicit(Voltage::V1_8)),
            [Some(Voltage::V1_8), None]
        );
    }

    #[test]
    fn auto_diagnostic_voltage_uses_safe_order() {
        assert_eq!(
            diagnostic_voltages(VoltageChoice::Auto),
            [Some(Voltage::V1_8), Some(Voltage::V3_3)]
        );
    }
}
