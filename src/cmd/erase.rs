use anyhow::{bail, Result};
use std::path::PathBuf;
use tracing::info;

use crate::bios::layout;
use crate::fpga;
use crate::spi::SpiSpeed;
use crate::{prepare, spi, VoltageChoice};

pub async fn cmd_erase(
    vc: VoltageChoice,
    speed: SpiSpeed,
    offset: Option<u32>,
    length: Option<u32>,
    layout: Option<PathBuf>,
    region: Option<String>,
) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    let result = (async {
        let (eff_offset, eff_length) = if let Some(ref rname) = region {
            let source = match &layout {
                Some(p) => layout::RegionSource::LayoutFile(p.clone()),
                None => layout::RegionSource::FmapScan,
            };
            let r = layout::resolve_region(source, rname, &chip, &dev, speed).await?;
            (Some(r.offset), Some(r.length))
        } else if layout.is_some() {
            let regions = layout::parse_layout_file(layout.as_ref().unwrap())?;
            eprintln!("Available regions:");
            for r in &regions { eprintln!("  {}", r.name); }
            bail!("--layout requires --region");
        } else {
            (offset, length)
        };

        match (eff_offset, eff_length) {
            (None, None) => {
                info!("chip erase: {} — this may take up to 200 seconds", chip.name);
                spi::erase_chip(&dev, &chip).await?;
                println!("Erased (chip)");
            }
            (off, len) => {
                let off = off.unwrap_or(0);
                if off >= chip.size_bytes {
                    bail!("offset {off:#x} exceeds chip size {:#x}", chip.size_bytes);
                }
                let max_len = chip.size_bytes - off;
                let len = match len {
                    Some(l) if l > max_len => {
                        bail!("length {l:#x} exceeds available space {max_len:#x} at offset {off:#x}")
                    }
                    Some(l) => l,
                    None => max_len,
                };
                info!("range erase: {off:#010x}..{:#010x}", off + len);
                spi::erase_range(&dev, &chip, off, len).await?;
                println!("Erased {} bytes at {off:#010x}", len);
            }
        }
        Ok(())
    }).await;
    fpga::vcc_off(&dev).await.ok();
    result
}
