use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::fpga::Voltage;
use crate::spi::{self, SpiSpeed};

pub async fn cmd_compare(
    voltage: Voltage,
    speed: SpiSpeed,
    file: PathBuf,
    offset: u32,
    length: Option<u32>,
) -> Result<()> {
    let expected =
        std::fs::read(&file).with_context(|| format!("failed to read {}", file.display()))?;

    let dev = crate::setup(voltage, speed).await?;
    let chip = spi::detect(&dev).await?.context("no chip detected")?;

    if offset >= chip.size_bytes {
        anyhow::bail!("offset {offset:#x} exceeds chip size {:#x}", chip.size_bytes);
    }
    let max_len = chip.size_bytes - offset;
    let len = match length {
        Some(l) if l > max_len => {
            anyhow::bail!("length {l:#x} exceeds available space {max_len:#x}")
        }
        Some(l) => l,
        None => expected.len().min(max_len as usize) as u32,
    };

    if expected.len() != len as usize {
        anyhow::bail!(
            "file is {} bytes but compare length is {} bytes",
            expected.len(),
            len
        );
    }

    let flash = spi::read(&dev, chip, offset, len).await?;

    let file_hash = hex(Sha256::digest(&expected));
    let flash_hash = hex(Sha256::digest(&flash));

    println!("File:  {file_hash}  {}", file.display());
    println!("Flash: {flash_hash}  (offset {offset:#010x}, {len} bytes)");

    if file_hash == flash_hash {
        println!("Match: OK");
        return Ok(());
    }

    println!("Match: FAIL");

    // show first 8 differing offsets
    let diffs: Vec<u32> = expected
        .iter()
        .zip(flash.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| offset + i as u32)
        .take(8)
        .collect();

    let total_diffs = expected.iter().zip(flash.iter()).filter(|(a, b)| a != b).count();
    println!("Diffs: {total_diffs} bytes differ");
    for addr in &diffs {
        let i = (addr - offset) as usize;
        println!("  {addr:#010x}  file={:#04x}  flash={:#04x}", expected[i], flash[i]);
    }
    if total_diffs > diffs.len() {
        println!("  ... ({} more)", total_diffs - diffs.len());
    }

    anyhow::bail!("verification failed")
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
