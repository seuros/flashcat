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

    /// Target voltage (1v8 or 3v3)
    #[arg(long, default_value = "3v3", global = true)]
    voltage: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check device connection and firmware version
    Check,

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

    /// Write file to flash (no erase — use erase first)
    Write {
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u32,
        /// Read back and verify after writing
        #[arg(long)]
        verify: bool,
    },

    /// Erase entire chip
    Erase,

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

    let voltage = match cli.voltage.as_str() {
        "1v8" | "1.8" => Voltage::V1_8,
        "3v3" | "3.3" => Voltage::V3_3,
        "5v" | "5.0"  => Voltage::V5_0,
        v => bail!("unknown voltage '{v}' — use 1v8, 3v3, or 5v"),
    };

    let speed = SpiSpeed(cli.mhz);

    match &cli.cmd {
        Cmd::Check => cmd::cmd_check().await,
        Cmd::Detect => cmd::cmd_detect(voltage, speed).await,
        Cmd::Read { file, offset, length, quad } => {
            cmd::cmd_read(voltage, speed, file.clone(), *offset, *length, *quad).await
        }
        Cmd::Write { file, offset, verify } => {
            cmd::cmd_write(voltage, speed, file.clone(), *offset, *verify).await
        }
        Cmd::Erase => cmd::cmd_erase(voltage, speed).await,
        Cmd::Compare { file, offset, length } => {
            cmd::cmd_compare(voltage, speed, file.clone(), *offset, *length).await
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
