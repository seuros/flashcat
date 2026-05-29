use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::spi::{otp_geometry, read_otp_locks, read_security_register};
use crate::spi::SpiSpeed;
use crate::{prepare, with_cleanup, VoltageChoice};

/// `flashcat otp read` — dump security register(s) to stdout or a file.
pub async fn cmd_otp_read(
    vc: VoltageChoice,
    speed: SpiSpeed,
    reg: Option<u8>,
    file: Option<PathBuf>,
) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    with_cleanup(&dev, async {
        let geom = otp_geometry(chip.mfr, chip.size_bytes)?;
        if let Some(r) = reg
            && (r == 0 || r > geom.count)
        {
            bail!("--reg must be 1..={} for {}", geom.count, chip.name);
        }

        let regs: Vec<u8> = match reg {
            Some(r) => vec![r],
            None => (1..=geom.count).collect(),
        };

        let locks = read_otp_locks(&dev).await.unwrap_or([false; 3]);
        let mut all = Vec::new();
        println!("Chip: {} — {} security register(s), {} bytes each, {}-byte address",
            chip.name, geom.count, geom.size, chip.addr_bytes);

        for r in &regs {
            let data = read_security_register(&dev, *r, chip.addr_bytes, geom.size).await?;
            let blank = data.iter().all(|&b| b == 0xFF) || data.iter().all(|&b| b == 0x00);
            let locked = locks.get((*r - 1) as usize).copied().unwrap_or(false);
            println!(
                "\nSecurity Register {r} [{}{}]:",
                if locked { "OTP-LOCKED" } else { "unlocked" },
                if blank { ", blank" } else { "" },
            );
            if file.is_none() {
                hexdump(&data, crate::spi::otp::register_base(*r));
            }
            all.extend_from_slice(&data);
        }

        if let Some(path) = file {
            std::fs::write(&path, &all)?;
            println!("\nWrote {} bytes to {}", all.len(), path.display());
        }

        Ok(())
    })
    .await
}

/// `flashcat otp lock-status` — show OTP lock bits LB1-3 (SR2 bits 3-5).
pub async fn cmd_otp_lock_status(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    let (dev, chip, _voltage) = prepare(vc, speed).await?;
    with_cleanup(&dev, async {
        let geom = otp_geometry(chip.mfr, chip.size_bytes)?;
        let locks = read_otp_locks(&dev).await?;
        println!("Chip: {}", chip.name);
        for r in 1..=geom.count {
            let locked = locks.get((r - 1) as usize).copied().unwrap_or(false);
            println!(
                "  LB{r}  security register {r}: {}",
                if locked { "OTP-LOCKED (permanent)" } else { "unlocked" }
            );
        }
        Ok(())
    })
    .await
}

fn hexdump(data: &[u8], base: u32) {
    for (i, chunk) in data.chunks(16).enumerate() {
        let addr = base as usize + i * 16;
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        println!("  {addr:06x}  {:<48}  {ascii}", hex.join(" "));
    }
}
