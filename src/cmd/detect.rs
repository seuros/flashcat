use anyhow::Result;

use crate::spi::SpiSpeed;
use crate::{prepare, VoltageChoice};

pub async fn cmd_detect(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    match prepare(vc, speed).await {
        Ok((_, chip, voltage)) => {
            println!("Chip:      {}", chip.name);
            println!("Size:      {} MB ({} bytes)", chip.size_bytes / 1024 / 1024, chip.size_bytes);
            println!("Page:      {} bytes", chip.page_size);
            println!("Erase:     {} bytes", chip.erase_size);
            println!("Addr:      {}-byte", chip.addr_bytes);
            println!("Voltage:   {:?}", voltage);
        }
        Err(e) if e.to_string().contains("no chip detected") => {
            println!("No chip detected");
        }
        Err(e) => return Err(e),
    }
    Ok(())
}
