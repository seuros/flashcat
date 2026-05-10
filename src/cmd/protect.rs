use anyhow::Result;

use crate::spi::{self, SpiSpeed};
use crate::{prepare, with_cleanup, VoltageChoice};

pub async fn cmd_protect(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    with_cleanup(&dev, async {
        spi::protect_chip(&dev, &chip).await?;
        let wp = spi::read_wp_status(&dev, &chip).await?;
        println!("Protected");
        println!("Status:  {}", wp.summary());
        Ok(())
    }).await
}

pub async fn cmd_unprotect(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    with_cleanup(&dev, async {
        spi::unprotect_chip(&dev, &chip).await?;
        let wp = spi::read_wp_status(&dev, &chip).await?;
        println!("Unprotected");
        println!("Status:  {}", wp.summary());
        Ok(())
    }).await
}
