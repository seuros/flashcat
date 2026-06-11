use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::fpga::{self, Voltage};
use crate::programmer::Programmer;
use crate::{pnor, usb, VoltageChoice};

/// Parallel NOR runs on the xPort (no FPGA) and the Mach1 (needs the generic
/// FPGA bitstream). Bring up the board, then hand back the open device.
async fn open(vc: VoltageChoice) -> Result<usb::UsbDevice> {
    // Parallel NOR is x16/3V (or 5V via the manual switch); auto-probe doesn't
    // apply, so resolve to 3.3V unless an explicit voltage was given.
    let voltage = match vc {
        VoltageChoice::Explicit(v) => v,
        VoltageChoice::Auto => Voltage::V3_3,
    };
    let dev = usb::connect().await?;
    match dev.kind {
        Programmer::Xport => {} // bare AVR, no bitstream
        Programmer::Mach1 => fpga::mach1_load_generic(&dev, voltage).await?,
        k => bail!(
            "parallel NOR requires the xPort or Mach1 (FPGA boards with a parallel bus); \
             {} does not route a parallel bus",
            k.name()
        ),
    }
    Ok(dev)
}

pub async fn cmd_pnor_detect(vc: VoltageChoice) -> Result<()> {
    let dev = open(vc).await?;
    let id = pnor::setup(&dev).await?;
    println!("Parallel NOR:");
    println!("  Manufacturer: {:#04x}", id.mfg);
    println!("  Device ID:    {:#06x} {:#04x}", id.id1, id.id2);
    Ok(())
}

pub async fn cmd_pnor_read(vc: VoltageChoice, file: PathBuf, offset: u32, length: u32) -> Result<()> {
    let dev = open(vc).await?;
    let id = pnor::setup(&dev).await?;
    println!("Parallel NOR mfg={:#04x} id={:#06x} — reading {length} bytes", id.mfg, id.id1);
    let data = pnor::read(&dev, offset, length).await?;
    std::fs::write(&file, &data)?;
    println!("Saved {} bytes → {}", data.len(), file.display());
    Ok(())
}

pub async fn cmd_pnor_erase(vc: VoltageChoice) -> Result<()> {
    let dev = open(vc).await?;
    pnor::setup(&dev).await?;
    pnor::chip_erase(&dev).await?;
    println!("Chip erased.");
    Ok(())
}

pub async fn cmd_pnor_write(
    vc: VoltageChoice,
    file: PathBuf,
    offset: u32,
    erase: bool,
    verify: bool,
) -> Result<()> {
    let data = std::fs::read(&file)?;
    let dev = open(vc).await?;
    pnor::setup(&dev).await?;
    if erase {
        pnor::chip_erase(&dev).await?;
        println!("Chip erased.");
    }
    pnor::write(&dev, offset, &data).await?;
    println!("Wrote {} bytes → offset {:#x}", data.len(), offset);
    if verify {
        let back = pnor::read(&dev, offset, data.len() as u32).await?;
        if back == data {
            println!("Verify:  OK");
        } else {
            let diff = back.iter().zip(&data).filter(|(a, b)| a != b).count();
            bail!("verify FAILED: {diff} mismatched bytes");
        }
    }
    Ok(())
}
