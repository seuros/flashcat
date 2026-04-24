use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::bios::layout;
use crate::spi::SpiSpeed;
use crate::{prepare, spi, VoltageChoice};

pub async fn cmd_read(
    vc: VoltageChoice,
    speed: SpiSpeed,
    file: PathBuf,
    offset: u32,
    length: Option<u32>,
    quad: bool,
    legacy_read: bool,
    layout: Option<PathBuf>,
    region: Option<String>,
) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;

    if quad && !chip.quad {
        bail!("{} does not support Quad SPI reads", chip.name);
    }
    if quad && dev.kind != crate::programmer::Programmer::Mach1 {
        bail!("--quad requires Mach1 hardware — Pro PCB5 does not route IO2/IO3 to the chip socket");
    }

    let (eff_offset, eff_len) = if let Some(ref rname) = region {
        let source = match &layout {
            Some(p) => layout::RegionSource::LayoutFile(p.clone()),
            None => layout::RegionSource::FmapScan,
        };
        let r = layout::resolve_region(source, rname, &chip, &dev, speed).await?;
        (r.offset, Some(r.length))
    } else if layout.is_some() {
        let regions = layout::parse_layout_file(layout.as_ref().unwrap())?;
        eprintln!("Available regions:");
        for r in &regions {
            eprintln!("  {}", r.name);
        }
        bail!("--layout requires --region");
    } else {
        (offset, length)
    };

    // validate range
    if eff_offset >= chip.size_bytes {
        bail!("offset {eff_offset:#x} exceeds chip size {:#x}", chip.size_bytes);
    }
    let max_len = chip.size_bytes - eff_offset;
    let len = match eff_len {
        Some(l) if l > max_len => {
            bail!("length {l:#x} exceeds available space {max_len:#x} at offset {eff_offset:#x}")
        }
        Some(l) => l,
        None => max_len,
    };

    info!("reading {} bytes from {} (offset {eff_offset:#010x})", len, chip.name);

    let data = if quad {
        info!("quad SPI mode: enabling QE bit and using SqiRdFlash");
        spi::enable_quad(&dev).await?;
        spi::sqi_setup(&dev, speed.0).await?;
        spi::read_quad(&dev, &chip, eff_offset, len).await?
    } else {
        spi::read(&dev, &chip, eff_offset, len, legacy_read).await?
    };

    std::fs::write(&file, &data).with_context(|| format!("failed to write {}", file.display()))?;
    println!("Saved {} bytes → {}", data.len(), file.display());
    Ok(())
}
