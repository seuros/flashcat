#![warn(clippy::all)]

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod bios;
mod chip;
mod cmd;
mod db;
mod fpga;
mod progress;
mod programmer;
mod spi;
mod usb;

pub(crate) use chip::ResolvedChip;

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
        /// Use legacy Read (0x03) instead of Fast Read (0x0B)
        #[arg(long)]
        legacy_read: bool,
        /// Layout file for region selection (flashrom format)
        #[arg(long, value_name = "FILE")]
        layout: Option<PathBuf>,
        /// Region name to read (requires --layout or uses FMAP scan)
        #[arg(long, value_name = "NAME")]
        region: Option<String>,
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
        /// Smart write: read-compare-erase-write (skips matching sectors and 0xFF pages)
        #[arg(long)]
        smart: bool,
        /// Layout file for region selection (flashrom format)
        #[arg(long, value_name = "FILE")]
        layout: Option<PathBuf>,
        /// Region name to write (requires --layout or uses FMAP scan)
        #[arg(long, value_name = "NAME")]
        region: Option<String>,
    },

    /// Erase flash (chip by default; --offset + --length for sector range)
    Erase {
        /// Start address (default: 0 = chip erase)
        #[arg(long, value_parser = parse_hex_or_dec)]
        offset: Option<u32>,
        /// Number of bytes to erase (rounded up to erase unit boundary)
        #[arg(long, value_parser = parse_hex_or_dec)]
        length: Option<u32>,
        /// Layout file for region selection (flashrom format)
        #[arg(long, value_name = "FILE")]
        layout: Option<PathBuf>,
        /// Region name to erase (requires --layout or uses FMAP scan)
        #[arg(long, value_name = "NAME")]
        region: Option<String>,
    },

    /// Read and decode SFDP (Serial Flash Discoverable Parameters)
    Sfdp,

    /// Compare flash contents against a file (SHA-256 + diff report)
    Compare {
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u32,
        #[arg(long, value_parser = parse_hex_or_dec)]
        length: Option<u32>,
        /// Layout file for region selection (flashrom format)
        #[arg(long, value_name = "FILE")]
        layout: Option<PathBuf>,
        /// Region name to compare (requires --layout or uses FMAP scan)
        #[arg(long, value_name = "NAME")]
        region: Option<String>,
    },

    /// Read FMAP region map from flash (or a local binary dump with --file)
    Fmap {
        /// Maximum bytes to scan for FMAP signature (hardware mode only)
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0x400000")]
        scan_limit: u32,
        /// Scan a local binary dump instead of reading hardware
        #[arg(short, long, value_name = "FILE")]
        file: Option<PathBuf>,
    },

    /// Read the chip's unique 64-bit serial number
    Uid,

    /// Parse a layout file and list regions (no hardware required)
    Regions {
        #[arg(short, long)]
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .without_time()
        .with_target(false)
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
        Cmd::Read { file, offset, length, quad, legacy_read, layout, region } => {
            cmd::cmd_read(
                vc, speed, file.clone(), *offset, *length, *quad, *legacy_read,
                layout.clone(), region.clone(),
            ).await
        }
        Cmd::Write { file, offset, erase, verify, smart, layout, region } => {
            cmd::cmd_write(
                vc, speed, file.clone(), *offset, *erase, *verify, *smart,
                layout.clone(), region.clone(),
            ).await
        }
        Cmd::Sfdp => cmd::cmd_sfdp(vc, speed).await,
        Cmd::Erase { offset, length, layout, region } => {
            cmd::cmd_erase(vc, speed, *offset, *length, layout.clone(), region.clone()).await
        }
        Cmd::Compare { file, offset, length, layout, region } => {
            cmd::cmd_compare(
                vc, speed, file.clone(), *offset, *length,
                layout.clone(), region.clone(),
            ).await
        }
        Cmd::Fmap { scan_limit, file } => cmd::cmd_fmap(vc, speed, *scan_limit, file.clone()).await,
        Cmd::Uid => cmd::cmd_uid(vc, speed).await,
        Cmd::Regions { file } => cmd::cmd_regions(file.clone()).await,
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
) -> Result<(usb::UsbDevice, ResolvedChip, Voltage)> {
    match vc {
        VoltageChoice::Auto => {
            let (dev, chip_opt, voltage) = spi::auto_probe(speed).await?;
            let chip = chip_opt.ok_or_else(|| anyhow::anyhow!("no chip detected"))?;
            Ok((dev, chip, voltage))
        }
        VoltageChoice::Explicit(voltage) => {
            let dev = setup(voltage, speed).await?;
            match spi::detect(&dev, voltage).await? {
                Some(chip) => Ok((dev, chip, voltage)),
                None => {
                    if let Err(e) = fpga::vcc_off(&dev).await {
                        tracing::warn!("vcc_off after no chip: {e}");
                    }
                    anyhow::bail!("no chip detected")
                }
            }
        }
    }
}
