use anyhow::Result;

use crate::fpga::Voltage;
use crate::spi::SpiSpeed;
use crate::{spi, setup};

pub async fn cmd_detect(voltage: Voltage, speed: SpiSpeed) -> Result<()> {
    let dev = setup(voltage, speed).await?;
    match spi::detect(&dev, voltage).await? {
        Some(chip) => {
            println!("Chip:      {}", chip.name);
            println!("Size:      {} MB ({} bytes)", chip.size_bytes / 1024 / 1024, chip.size_bytes);
            println!("Page:      {} bytes", chip.page_size);
            println!("Erase:     {} bytes", chip.erase_size);
            println!("Addr:      {}-byte", chip.addr_bytes);
        }
        None => {
            let id = spi::rdid(&dev).await?;
            println!(
                "RDID:      {:#04x} {:#04x} {:#04x} — not in database",
                id[0], id[1], id[2]
            );
        }
    }
    Ok(())
}
