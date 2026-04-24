#![warn(clippy::all)]

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod cmd;
mod db;
mod fpga;
mod progress;
mod programmer;
mod spi;
mod usb;

use fpga::Voltage;
use spi::SpiSpeed;

#[derive(Clone, Copy)]
pub(crate) enum VoltageChoice {
    Auto,
    Explicit(Voltage),
}

fn parse_mhz(s: &str) -> Result<u8, String> {
    let mhz: u8 = s.parse().map_err(|_| format!("'{s}' is not a valid MHz value"))?;
    if SpiSpeed::ALL.contains(&SpiSpeed(mhz)) {
        Ok(mhz)
    } else {
        Err(format!("'{mhz}' is not supported — use one of: 1, 2, 4, 8, 12, 16, 24, 32"))
    }
}

fn parse_hex_or_dec(s: &str) -> Result<u32, String> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

#[derive(Parser)]
#[command(name = "flashcat", version, about = "FlashcatUSB Pro — Linux/FreeBSD/macOS")]
struct Cli {
    /// SPI clock in MHz (1, 2, 4, 8, 12, 16, 24, 32)
    #[arg(long, default_value = "8", global = true, value_parser = parse_mhz)]
    mhz: u8,

    /// Target voltage: auto (default), 1v8, 3v3, or 5v
    #[arg(long, default_value = "auto", global = true)]
    voltage: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check device connection and firmware version
    Check,

    /// Watch for FlashcatUSB plug-in events and auto-detect chip
    Watch,

    /// Detect and identify the attached SPI NOR chip
    Detect,

    /// Read flash to file
    Read {
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u32,
        #[arg(long, value_parser = parse_hex_or_dec)]
        length: Option<u32>,
        /// Use Quad SPI (4-bit) read path (chip must support quad mode)
        #[arg(long)]
        quad: bool,
    },

    /// Write file to flash
    Write {
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u32,
        /// Erase affected sectors before writing
        #[arg(long)]
        erase: bool,
        /// Read back and verify after writing
        #[arg(long)]
        verify: bool,
    },

    /// Erase flash (chip by default; --offset + --length for sector range)
    Erase {
        /// Start address (default: 0 = chip erase)
        #[arg(long, value_parser = parse_hex_or_dec)]
        offset: Option<u32>,
        /// Number of bytes to erase (rounded up to erase unit boundary)
        #[arg(long, value_parser = parse_hex_or_dec)]
        length: Option<u32>,
    },

    /// Compare flash contents against a file (SHA-256 + diff report)
    Compare {
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u32,
        #[arg(long, value_parser = parse_hex_or_dec)]
        length: Option<u32>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    let vc = match cli.voltage.as_str() {
        "auto"        => VoltageChoice::Auto,
        "1v8" | "1.8" => VoltageChoice::Explicit(Voltage::V1_8),
        "3v3" | "3.3" => VoltageChoice::Explicit(Voltage::V3_3),
        "5v"  | "5.0" => VoltageChoice::Explicit(Voltage::V5_0),
        v => bail!("unknown voltage '{v}' — use auto, 1v8, 3v3, or 5v"),
    };

    let speed = SpiSpeed(cli.mhz);

    match &cli.cmd {
        Cmd::Check => cmd::cmd_check().await,
        Cmd::Watch => cmd::cmd_watch(vc, speed).await,
        Cmd::Detect => cmd::cmd_detect(vc, speed).await,
        Cmd::Read { file, offset, length, quad } => {
            cmd::cmd_read(vc, speed, file.clone(), *offset, *length, *quad).await
        }
        Cmd::Write { file, offset, erase, verify } => {
            cmd::cmd_write(vc, speed, file.clone(), *offset, *erase, *verify).await
        }
        Cmd::Erase { offset, length } => cmd::cmd_erase(vc, speed, *offset, *length).await,
        Cmd::Compare { file, offset, length } => {
            cmd::cmd_compare(vc, speed, file.clone(), *offset, *length).await
        }
    }
}

pub(crate) async fn setup(voltage: Voltage, speed: SpiSpeed) -> Result<usb::UsbDevice> {
    let dev = usb::connect().await?;
    if !dev.kind.supports_voltage(voltage) {
        bail!(
            "{:?} does not support {:?} — supported: {:?}",
            dev.kind, voltage, dev.kind.supported_voltages()
        );
    }
    fpga::load(&dev, voltage).await?;
    fpga::set_vcc(&dev, voltage).await?;
    spi::init(&dev, speed).await?;
    Ok(dev)
}

/// Unified prepare: resolve voltage (auto-probing if needed), return configured device + chip.
pub(crate) async fn prepare(
    vc: VoltageChoice,
    speed: SpiSpeed,
) -> Result<(usb::UsbDevice, &'static db::SpiNorDef, Voltage)> {
    match vc {
        VoltageChoice::Auto => {
            let (dev, chip_opt, voltage) = spi::auto_probe(speed).await?;
            let chip = chip_opt.ok_or_else(|| anyhow::anyhow!("no chip detected"))?;
            Ok((dev, chip, voltage))
        }
        VoltageChoice::Explicit(voltage) => {
            let dev = setup(voltage, speed).await?;
            let chip = spi::detect(&dev, voltage)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no chip detected"))?;
            Ok((dev, chip, voltage))
        }
    }
}
