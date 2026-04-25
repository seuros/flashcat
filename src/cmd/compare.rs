use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::bios::layout;
use crate::fpga;
use crate::spi::{self, SpiSpeed};
use crate::{prepare, VoltageChoice};

pub struct CompareOpts {
    pub vc: VoltageChoice,
    pub speed: SpiSpeed,
    pub file: PathBuf,
    pub offset: u32,
    pub length: Option<u32>,
    pub layout: Option<PathBuf>,
    pub region: Option<String>,
}

pub async fn cmd_compare(opts: CompareOpts) -> Result<()> {
    let expected = std::fs::read(&opts.file)
        .with_context(|| format!("failed to read {}", opts.file.display()))?;
    let (dev, chip, _voltage) = prepare(opts.vc, opts.speed).await?;
    let result = (async {
        let (eff_offset, eff_length) = if let Some(ref rname) = opts.region {
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
            (opts.offset, opts.length)
        };

        if eff_offset >= chip.size_bytes {
            anyhow::bail!("offset {eff_offset:#x} exceeds chip size {:#x}", chip.size_bytes);
        }
        let max_len = chip.size_bytes - eff_offset;
        let len = match eff_length {
            Some(l) if l > max_len => anyhow::bail!("length {l:#x} exceeds available space {max_len:#x}"),
            Some(l) => l,
            None => expected.len().min(max_len as usize) as u32,
        };

        if expected.len() != len as usize {
            anyhow::bail!("file is {} bytes but compare length is {} bytes", expected.len(), len);
        }

        let flash = spi::read(&dev, &chip, eff_offset, len, false).await?;
        let file_hash = hex(Sha256::digest(&expected));
        let flash_hash = hex(Sha256::digest(&flash));

        println!("File:  {file_hash}  {}", opts.file.display());
        println!("Flash: {flash_hash}  (offset {eff_offset:#010x}, {len} bytes)");

        if file_hash == flash_hash {
            println!("Match: OK");
            return Ok(());
        }

        println!("Match: FAIL");
        let diffs: Vec<u32> = expected.iter().zip(flash.iter()).enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| eff_offset + i as u32)
            .take(8).collect();
        let total_diffs = expected.iter().zip(flash.iter()).filter(|(a, b)| a != b).count();
        println!("Diffs: {total_diffs} bytes differ");
        for addr in &diffs {
            let i = (addr - eff_offset) as usize;
            println!("  {addr:#010x}  file={:#04x}  flash={:#04x}", expected[i], flash[i]);
        }
        if total_diffs > diffs.len() {
            println!("  ... ({} more)", total_diffs - diffs.len());
        }
        anyhow::bail!("verification failed")
    }).await;
    fpga::vcc_off(&dev).await.ok();
    result
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
