use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::bios::{amd_psp, efifv, ifd, layout};
use crate::fpga;
use crate::spi::SpiSpeed;
use crate::{prepare, spi, VoltageChoice};

pub async fn cmd_fmap(
    vc: VoltageChoice,
    speed: SpiSpeed,
    scan_limit: u32,
    from_file: Option<PathBuf>,
) -> Result<()> {
    let (data, source_name, flash_size) = match from_file {
        Some(ref path) => {
            println!("Scanning {} ...", path.display());
            let bytes = std::fs::read(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let sz = bytes.len() as u32;
            (bytes, path.display().to_string(), Some(sz))
        }
        None => {
            let (dev, chip, _voltage) = prepare(vc, speed).await?;
            let actual_limit = scan_limit.min(chip.size_bytes);
            println!("Scanning {} bytes of {} ...", actual_limit, chip.name);
            let result = spi::read(&dev, &chip, 0, actual_limit, false).await;
            let sz = chip.size_bytes;
            let name = chip.name.clone();
            fpga::vcc_off(&dev).await.ok();
            (result?, name, Some(sz))
        }
    };

    // Try FMAP first, then IFD.
    if let Some((hdr, areas)) = layout::scan_fmap(&data) {
        println!();
        println!("Format:  FMAP ({})", source_name);
        println!("  version : {}.{}", hdr.ver_major, hdr.ver_minor);
        println!("  base    : {:#018x}", hdr.base);
        println!("  size    : {:#010x} ({} bytes)", hdr.size, hdr.size);
        println!("  name    : {}", hdr.name);
        println!("  areas   : {}", hdr.nareas);
        println!();

        if areas.is_empty() {
            println!("(no areas defined)");
            return Ok(());
        }

        println!("{:<24} {:<12} {:<10}  FLAGS", "NAME", "OFFSET", "SIZE");
        println!("{}", "-".repeat(64));
        for area in &areas {
            println!(
                "  {:<24} {:#010x}  {:<10}  {}",
                area.name, area.offset, area.size,
                format_flags(area.flags),
            );
        }
        return Ok(());
    }

    if let Some(info) = ifd::scan_ifd(&data) {
        println!();
        println!("Format:  Intel IFD ({})", source_name);
        ifd::print_ifd(&info, flash_size);
        return Ok(());
    }

    // AMD PSP before EFI FV — AMD images contain EFI volumes in the BIOS partition,
    // so EFI FV would falsely match if checked first.
    if let Some(info) = amd_psp::scan_amd_psp(&data) {
        println!();
        println!("Format:  AMD PSP ({})", source_name);
        amd_psp::print_amd_psp(&info, data.len() as u32);
        return Ok(());
    }

    if let Some(info) = efifv::scan_efifv(&data) {
        println!();
        println!("Format:  EFI Firmware Volume ({})", source_name);
        efifv::print_efifv(&info, data.len() as u32);
        return Ok(());
    }

    anyhow::bail!("no recognized firmware map found (tried FMAP, Intel IFD, EFI FV, AMD PSP)");
}

fn format_flags(flags: u16) -> String {
    let mut parts = Vec::new();
    if flags & (1 << 0) != 0 { parts.push("STATIC"); }
    if flags & (1 << 1) != 0 { parts.push("COMPRESSED"); }
    if flags & (1 << 2) != 0 { parts.push("RO"); }
    if parts.is_empty() { format!("{flags:#06x}") } else { parts.join("|") }
}
