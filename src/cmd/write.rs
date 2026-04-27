use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use tracing::info;

use crate::bios::layout;
use crate::fpga;
use crate::spi::SpiSpeed;
use crate::{VoltageChoice, prepare, spi};

use super::compare::probable_missing_erase;

pub struct WriteOpts {
    pub vc: VoltageChoice,
    pub speed: SpiSpeed,
    pub file: PathBuf,
    pub offset: u32,
    pub erase: bool,
    pub verify: bool,
    pub smart: bool,
    pub layout: Option<PathBuf>,
    pub region: Option<String>,
}

pub async fn cmd_write(opts: WriteOpts) -> Result<()> {
    let (dev, chip, _voltage) = prepare(opts.vc, opts.speed).await?;
    let result = (async {
        let data = std::fs::read(&opts.file)
            .with_context(|| format!("failed to read {}", opts.file.display()))?;

        let (eff_offset, eff_len) = if let Some(ref rname) = opts.region {
            let source = match &opts.layout {
                Some(p) => layout::RegionSource::LayoutFile(p.clone()),
                None => layout::RegionSource::FmapScan,
            };
            let r = layout::resolve_region(source, rname, &chip, &dev, opts.speed).await?;
            (r.offset, Some(r.length))
        } else if let Some(ref lpath) = opts.layout {
            let regions = layout::parse_layout_file(lpath)?;
            eprintln!("Available regions:");
            for r in &regions { eprintln!("  {}", r.name); }
            bail!("--layout requires --region");
        } else {
            (opts.offset, None)
        };

        if let Some(region_len) = eff_len
            && data.len() != region_len as usize { bail!(
                "file is {} bytes but region is {} bytes — sizes must match for region write",
                data.len(), region_len
            ); }

        if eff_offset >= chip.size_bytes {
            bail!("offset {eff_offset:#x} exceeds chip size {:#x}", chip.size_bytes);
        }
        let available = (chip.size_bytes - eff_offset) as usize;
        if data.len() > available {
            bail!(
                "file ({} bytes) exceeds available space ({available} bytes at offset {eff_offset:#x})",
                data.len()
            );
        }

        if opts.smart {
            info!("smart write: read-compare-erase-write {} bytes to {} at {eff_offset:#010x}", data.len(), chip.name, );
            spi::write_smart(&dev, &chip, eff_offset, &data).await?;
            println!("Written {} bytes (smart)", data.len());
        } else {
            let full_chip = eff_offset == 0 && data.len() as u32 == chip.size_bytes;
            if opts.erase || full_chip {
                if full_chip {
                    info!("chip erase: {} — this may take up to {}s", chip.name, chip.chip_erase_timeout_secs());
                    spi::erase_chip(&dev, &chip).await?;
                    println!("Erased (chip)");
                } else {
                    spi::erase_range(&dev, &chip, eff_offset, data.len() as u32).await?;
                }
            }
            info!("writing {} bytes to {} at offset {eff_offset:#010x}", data.len(), chip.name);
            spi::write(&dev, &chip, eff_offset, &data).await?;
            println!("Written {} bytes", data.len());
        }

        if opts.verify {
            info!("verifying...");
            let readback = spi::read(&dev, &chip, eff_offset, data.len() as u32, false).await?;
            if readback != data {
                let diffs = data.iter().zip(readback.iter()).filter(|(a, b)| a != b).count();
                if probable_missing_erase(&data, &readback) {
                    bail!(
                        "verify failed — {diffs} bytes differ; readback only has bits cleared relative to the file, so the flash was probably not erased first. Re-write with --erase --verify or --smart --verify"
                    );
                }
                bail!("verify failed — {diffs} bytes differ");
            }
            println!("Verify:  OK");
        }
        Ok(())
    }).await;
    fpga::vcc_off(&dev).await.ok();
    result
}
