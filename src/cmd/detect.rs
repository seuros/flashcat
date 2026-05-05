use anyhow::Result;

use crate::chip::ParamSource;
use crate::spi::{read_wp_status, SpiSpeed};
use crate::{power_down_and_vcc_off, prepare, VoltageChoice};

pub async fn cmd_detect(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    match prepare(vc, speed).await {
        Ok((dev, chip, voltage)) => {
            println!("Chip:      {}", chip.name);
            println!("Size:      {} MB ({} bytes)", chip.size_bytes / 1024 / 1024, chip.size_bytes);
            println!("Page:      {} bytes", chip.page_size);
            println!("Erase:     {} bytes", chip.erase_size);
            println!("Addr:      {}-byte", chip.addr_bytes);
            println!("Voltage:   {:?}", voltage);
            println!("SFDP:      {}", match chip.source {
                ParamSource::DatabaseWithSfdp => "yes",
                ParamSource::Sfdp => "yes (no DB match)",
                ParamSource::Database => "no",
            });
            match read_wp_status(&dev, &chip).await {
                Ok(wp) => println!("WP:        {}", wp.summary()),
                Err(e) => println!("WP:        unavailable ({e})"),
            }
            if chip.source == ParamSource::Database {
                eprintln!("\x1b[31m⚠ SFDP absent — possible counterfeit chip\x1b[0m");
            }
            power_down_and_vcc_off(&dev).await;
            Ok(())
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
