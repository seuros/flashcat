use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::bios::layout;
use crate::fpga;
use crate::spi::SpiSpeed;
use crate::{prepare, spi, VoltageChoice};

pub struct ReadOpts {
    pub vc: VoltageChoice,
    pub speed: SpiSpeed,
    pub file: PathBuf,
    pub offset: u32,
    pub length: Option<u32>,
    pub quad: bool,
    pub legacy_read: bool,
    pub layout: Option<PathBuf>,
    pub region: Option<String>,
}

pub async fn cmd_read(opts: ReadOpts) -> Result<()> {
    let (dev, chip, _voltage) = prepare(opts.vc, opts.speed).await?;
    let result = run(&dev, &chip, &opts).await;
    fpga::vcc_off(&dev).await.ok();
    result
}

async fn run(
    dev: &crate::usb::UsbDevice,
    chip: &crate::ResolvedChip,
    opts: &ReadOpts,
) -> Result<()> {
    if opts.quad && !chip.quad {
        bail!("{} does not support Quad SPI reads", chip.name);
    }
    if opts.quad && dev.kind != crate::programmer::Programmer::Mach1 {
        bail!("--quad requires Mach1 hardware — Pro PCB5 does not route IO2/IO3 to the chip socket");
    }

    let (eff_offset, eff_len) = if let Some(ref rname) = opts.region {
        let source = match &opts.layout {
            Some(p) => layout::RegionSource::LayoutFile(p.clone()),
            None => layout::RegionSource::FmapScan,
        };
        let r = layout::resolve_region(source, rname, chip, dev, opts.speed).await?;
        (r.offset, Some(r.length))
    } else if let Some(ref lpath) = opts.layout {
        let regions = layout::parse_layout_file(lpath)?;
        eprintln!("Available regions:");
        for r in &regions { eprintln!("  {}", r.name); }
        bail!("--layout requires --region");
    } else {
        (opts.offset, opts.length)
    };

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

    let data = if opts.quad {
        info!("quad SPI mode: enabling QE bit and using SqiRdFlash");
        spi::enable_quad(dev).await?;
        spi::sqi_setup(dev, opts.speed.0).await?;
        spi::read_quad(dev, chip, eff_offset, len).await?
    } else {
        spi::read(dev, chip, eff_offset, len, opts.legacy_read).await?
    };

    std::fs::write(&opts.file, &data)
        .with_context(|| format!("failed to write {}", opts.file.display()))?;
    println!("Saved {} bytes → {}", data.len(), opts.file.display());
    Ok(())
}
