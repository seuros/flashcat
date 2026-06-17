use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::bios::layout;
use crate::spi::{self, SpiSpeed};
use crate::{prepare, with_cleanup, VoltageChoice};

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
    /// Number of read passes for majority-vote bit-flip recovery (1 = disabled, 3–100 active).
    pub passes: u32,
}

pub async fn cmd_read(opts: ReadOpts) -> Result<()> {
    let (dev, chip, _voltage) = prepare(opts.vc, opts.speed).await?;
    with_cleanup(&dev, run(&dev, &chip, &opts)).await
}

async fn run(
    dev: &crate::usb::UsbDevice,
    chip: &crate::ResolvedChip,
    opts: &ReadOpts,
) -> Result<()> {
    if opts.quad && !chip.quad {
        bail!("{} does not support Quad SPI reads", chip.name);
    }
    if opts.quad
        && !matches!(
            dev.kind,
            crate::programmer::Programmer::Pro5 | crate::programmer::Programmer::Xport
        )
    {
        bail!("--quad is supported on the Pro and xPort (the Classic and Mach1 firmware return zeros in quad mode); use a single-lane read there");
    }

    let (eff_offset, eff_len) =
        match layout::resolve_region_flags(opts.region.as_deref(), opts.layout.as_deref(), chip, dev, opts.speed).await? {
            Some((off, len)) => (off, Some(len)),
            None => (opts.offset, opts.length),
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
        // Validate the SQI clock speed against the programmer first — sqi_setup
        // only configures the FlashcatUSB programmer's clock divisor (UsbReq::SqiSetup)
        // and does not touch the flash chip.  Failing here avoids writing the QE
        // bit (potentially non-volatile) when an unsupported speed was requested.
        spi::sqi_setup(dev, opts.speed.0).await?;
        spi::enable_quad(dev, chip.mfr).await?;
        if chip.addr_bytes == 4 {
            spi::enter_4byte_mode(dev).await?; // EN4B (0xB7) — vendor sends this for 4-byte parts
        }
        spi::read_quad(dev, chip, eff_offset, len).await?
    } else if opts.passes > 1 {
        spi::majority_read(dev, chip, eff_offset, len, opts.legacy_read, opts.passes).await?
    } else {
        spi::read(dev, chip, eff_offset, len, opts.legacy_read).await?
    };

    std::fs::write(&opts.file, &data)
        .with_context(|| format!("failed to write {}", opts.file.display()))?;
    println!("Saved {} bytes → {}", data.len(), opts.file.display());
    Ok(())
}
