use anyhow::Result;

use crate::chip::ParamSource;
use crate::spi::{read_wp_status, SpiSpeed};
use crate::{prepare, with_cleanup, VoltageChoice};

pub async fn cmd_detect(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    match prepare(vc, speed).await {
        Ok((dev, chip, voltage)) => {
            with_cleanup(&dev, async {
                println!("Chip:      {}", chip.name);
                println!("Size:      {} MB ({} bytes)", chip.size_bytes / 1024 / 1024, chip.size_bytes);
                println!("Page:      {} bytes", chip.page_size);
                println!("Erase:     {} bytes", chip.erase_size);
                println!("Addr:      {}-byte", chip.addr_bytes);
                println!("Voltage:   {:?}", voltage);
                println!("SFDP:      {}", match chip.source {
                    ParamSource::DatabaseWithSfdp => "yes",
                    ParamSource::DatabaseWithPartialSfdp => "partial (pre-JESD216A)",
                    ParamSource::Sfdp => "yes (no DB match)",
                    ParamSource::Database => "no",
                });
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
            }).await
        }
        // NOTE: matches the literal string from anyhow::bail! in prepare() (main.rs).
        // This is a string-match sentinel; if the bail message ever changes this branch
        // will silently stop matching and the error will propagate as Err(e) below.
        Err(e) if e.to_string().contains("no chip detected") => {
            println!("No chip detected");
            Ok(())
        }
        Err(e) => Err(e),
    }
}
