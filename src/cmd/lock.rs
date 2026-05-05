use anyhow::{bail, Result};

use crate::spi::{self, SpiSpeed};
use crate::{power_down_and_vcc_off, prepare, VoltageChoice};

pub async fn cmd_block_lock(
    vc: VoltageChoice,
    speed: SpiSpeed,
    global: bool,
    addr: Option<u32>,
) -> Result<()> {
    match (global, addr) {
        (true, Some(_)) => bail!("--global and --addr are mutually exclusive"),
        (false, None) => bail!("specify --global or --addr ADDRESS"),
        _ => {}
    }
    let (dev, chip, _) = prepare(vc, speed).await?;
    let result = (async {
        if global {
            spi::global_lock(&dev, &chip).await?;
            let wp = spi::read_wp_status(&dev, &chip).await?;
            println!("All blocks locked");
            if !wp.block_lock_mode {
                println!("  note: WPS=0 — block lock bits set but not active; set SR3.WPS=1 to use per-block protection");
            }
            println!("  note: lock bits are volatile — cleared on power cycle or reset");
            println!("  note: flashcat cuts VCC after this command; bits persist only on externally-powered chips");
        } else if let Some(a) = addr {
            spi::lock_block(&dev, &chip, a).await?;
            let locked = spi::read_block_lock(&dev, &chip, a).await?;
            let wp = spi::read_wp_status(&dev, &chip).await?;
            println!("Block at {a:#010x}: {}", if locked { "locked" } else { "not locked" });
            if !wp.block_lock_mode {
                println!("  note: WPS=0 — individual block lock bits are set but BP-range protection (SR1) is active");
                println!("  set SR3.WPS=1 to enable per-block protection");
            }
            println!("  note: lock bits are volatile — cleared on power cycle or reset");
            println!("  note: flashcat cuts VCC after this command; bits persist only on externally-powered chips");
        }
        Ok(())
    })
    .await;
    power_down_and_vcc_off(&dev).await;
    result
}

pub async fn cmd_block_unlock(
    vc: VoltageChoice,
    speed: SpiSpeed,
    global: bool,
    addr: Option<u32>,
) -> Result<()> {
    match (global, addr) {
        (true, Some(_)) => bail!("--global and --addr are mutually exclusive"),
        (false, None) => bail!("specify --global or --addr ADDRESS"),
        _ => {}
    }
    let (dev, chip, _) = prepare(vc, speed).await?;
    let result = (async {
        if global {
            spi::global_unlock(&dev, &chip).await?;
            let wp = spi::read_wp_status(&dev, &chip).await?;
            println!("All blocks unlocked");
            if !wp.block_lock_mode {
                println!("  note: WPS=0 — individual block lock bits are not active (BP-range protection via SR1 is in effect)");
            }
            println!("  note: lock bits are volatile — cleared on power cycle or reset");
            println!("  note: flashcat cuts VCC after this command; bits persist only on externally-powered chips");
        } else if let Some(a) = addr {
            spi::unlock_block(&dev, &chip, a).await?;
            let locked = spi::read_block_lock(&dev, &chip, a).await?;
            let wp = spi::read_wp_status(&dev, &chip).await?;
            println!("Block at {a:#010x}: {}", if locked { "locked" } else { "unlocked" });
            if !wp.block_lock_mode {
                println!("  note: WPS=0 — individual block lock bits are not active (BP-range protection via SR1 is in effect)");
            }
            println!("  note: lock bits are volatile — cleared on power cycle or reset");
            println!("  note: flashcat cuts VCC after this command; bits persist only on externally-powered chips");
        }
        Ok(())
    })
    .await;
    power_down_and_vcc_off(&dev).await;
    result
}
