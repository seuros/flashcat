#![warn(clippy::all)]

use anyhow::{bail, Result};
use std::path::PathBuf;

mod bios;
mod chip;
mod cmd;
mod db;
mod fpga;
mod jtag;
mod memtest;
mod pnor;
mod progress;
mod programmer;
mod spi;
mod units;
mod usb;

pub(crate) use chip::ResolvedChip;

use fpga::Voltage;
use spi::SpiSpeed;

#[derive(Clone, Copy)]
pub(crate) enum VoltageChoice {
    Auto,
    Explicit(Voltage),
}

/// SPI clock in MHz, restricted to the rates the programmer supports.
#[derive(Clone, Copy)]
struct Mhz(u8);

impl std::str::FromStr for Mhz {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let mhz: u8 = s.parse().map_err(|_| format!("'{s}' is not a valid MHz value"))?;
        if SpiSpeed::ALL.contains(&SpiSpeed(mhz)) {
            Ok(Mhz(mhz))
        } else {
            Err(format!("'{mhz}' is not supported — use one of: 1, 2, 4, 8, 12, 16, 24, 32"))
        }
    }
}

/// An integer given as decimal or `0x`-prefixed hex.
#[derive(Clone, Copy, Debug)]
struct Hex<T>(T);

impl<T: TryFrom<u64>> std::str::FromStr for Hex<T> {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        bios::layout::parse_hex_or_dec(s).map(Hex)
    }
}

#[derive(usage::Cli)]
#[usage(
    bin = "flashcat",
    version,
    completion,
    about = "FlashcatUSB Pro — Linux/FreeBSD/macOS (voltage auto-detected by default)",
    example("flashcat detect"),
    example("flashcat read --file dump.bin"),
    example("flashcat write --file dump.bin --verify"),
    example("flashcat compare --file dump.bin"),
    after_help = "Sensible defaults — no global flags needed. Override only if you know you need to: \
                  --voltage auto-probes 1v8/3v3 (default: auto); \
                  --mhz is the SPI clock, default 8 MHz — raise to 16/24/32 if your wiring is clean."
)]
struct Cli {
    /// SPI clock in MHz — optional; default 8 MHz works for most chips
    #[usage(long, default = "8", global,
          value_name = "MHZ", help = "SPI clock (1,2,4,8,12,16,24,32) — optional, default 8")]
    mhz: Mhz,

    /// Target voltage — optional; 'auto' probes the chip
    #[usage(long, default = "auto", global,
          value_name = "V", help = "Target voltage: auto|1v8|3v3|5v — optional, default auto")]
    voltage: String,

    /// Programmer to use when several are attached (flashrom-style).
    /// Value: model (classic|xport|mach1|pro), serial=<s>, path=<busnum-port.chain>,
    /// or a bare serial/path. With one programmer attached this is unnecessary.
    #[usage(short = 'p', long = "programmer", global, value_name = "SEL",
          help = "Select programmer when >1 attached: classic|xport|mach1|pro, serial=…, path=…")]
    programmer: Option<String>,

    #[usage(subcommand)]
    cmd: Cmd,
}

#[derive(usage::Subcommands)]
enum Cmd {
    /// Check device connection and firmware version
    Check,

    /// List all attached FlashcatUSB programmers (model, USB path, serial, fw)
    Devices,

    /// Watch for FlashcatUSB plug-in events and auto-detect chip
    Watch,

    /// Detect and identify the attached SPI NOR chip
    Detect,

    /// Read flash to file
    Read {
        /// Output file (binary dump of flash contents)
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: PathBuf,
        #[usage(long, default = "0")]
        offset: Hex<u32>,
        #[usage(long, )]
        length: Option<Hex<u32>>,
        /// Use Quad SPI (4-bit) read path (chip must support quad mode)
        #[usage(long)]
        quad: bool,
        /// Use legacy Read (0x03) instead of Fast Read (0x0B)
        #[usage(long)]
        legacy_read: bool,
        /// Layout file for region selection (flashrom format)
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        layout: Option<PathBuf>,
        /// Region name to read (requires --layout or uses FMAP scan)
        #[usage(long, value_name = "NAME")]
        region: Option<String>,
        /// Read N times and use majority voting to recover from defective cells.
        /// --read-repeated (no value) defaults to 3. Range: 3–100.
        #[usage(
            long,
            value_name = "N",
            default_missing = "3",
            help = "Read N times (3–100, default 3) and majority-vote each bit (matches flashrom API)"
        )]
        read_repeated: Option<u32>,
    },

    /// Write file to flash (auto-detects voltage; --erase/--verify optional)
    Write {
        /// Input binary to flash
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: PathBuf,
        #[usage(long, default = "0")]
        offset: Hex<u32>,
        /// Force erase before writing, then raw write (bypasses smart comparison; use for pre-erased blank chips)
        #[usage(long)]
        erase: bool,
        /// Read back and verify after writing
        #[usage(long)]
        verify: bool,
        /// Deprecated: smart write is now always the default; this flag has no effect
        #[usage(long, hide)]
        smart: bool,
        /// Layout file for region selection (flashrom format)
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        layout: Option<PathBuf>,
        /// Region name to write (requires --layout or uses FMAP scan)
        #[usage(long, value_name = "NAME")]
        region: Option<String>,
    },

    /// Erase flash (chip by default; --offset + --length for sector range)
    Erase {
        /// Start address (default: 0 = chip erase)
        #[usage(long, )]
        offset: Option<Hex<u32>>,
        /// Number of bytes to erase (rounded up to erase unit boundary)
        #[usage(long, )]
        length: Option<Hex<u32>>,
        /// Layout file for region selection (flashrom format)
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        layout: Option<PathBuf>,
        /// Region name to erase (requires --layout or uses FMAP scan)
        #[usage(long, value_name = "NAME")]
        region: Option<String>,
    },

    /// Destructive self-test: erase/program/verify patterns to find bad sectors and fake-capacity chips
    #[usage(effect = "destructive")]
    Memtest {
        #[usage(long)]
        offset: Option<Hex<u32>>,
        #[usage(long)]
        length: Option<Hex<u32>>,
        /// Layout file for region selection (flashrom format)
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        layout: Option<PathBuf>,
        /// Region name to test (requires --layout or uses FMAP scan)
        #[usage(long, value_name = "NAME")]
        region: Option<String>,
        /// Save current contents here first; restore and verify them after the test
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        backup: Option<PathBuf>,
        /// Run on a non-blank range without a backup (contents are destroyed)
        #[usage(long)]
        force: bool,
        /// Add a pseudo-random data pass
        #[usage(long)]
        thorough: bool,
        /// Seed for the random pass (default: time-based, printed)
        #[usage(long, requires = "--thorough")]
        seed: Option<Hex<u64>>,
        /// Write a JSON report
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        report: Option<PathBuf>,
    },

    /// Read and decode SFDP (Serial Flash Discoverable Parameters)
    Sfdp,

    /// Compare flash contents against a file (SHA-256 + diff report)
    Compare {
        /// Reference binary to compare flash against
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: PathBuf,
        #[usage(long, default = "0")]
        offset: Hex<u32>,
        #[usage(long, )]
        length: Option<Hex<u32>>,
        /// Layout file for region selection (flashrom format)
        #[usage(long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        layout: Option<PathBuf>,
        /// Region name to compare (requires --layout or uses FMAP scan)
        #[usage(long, value_name = "NAME")]
        region: Option<String>,
    },

    /// Read FMAP region map from flash (or a local binary dump with --file)
    Fmap {
        /// Maximum bytes to scan for FMAP signature (hardware mode only)
        #[usage(long, default = "0x400000")]
        scan_limit: Hex<u32>,
        /// Scan a local binary dump instead of reading hardware
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: Option<PathBuf>,
    },

    /// Read the chip's unique 64-bit serial number
    Uid,

    /// Decode SR1/SR2/SR3 status registers (per-chip)
    Status,

    /// Protect entire chip (sets BP=all, survives power cycle)
    Protect,

    /// Remove all write protection (clears BP bits)
    Unprotect,

    /// Lock flash blocks (Winbond individual block lock, 0x36/0x7E)
    BlockLock {
        /// Lock all blocks globally (0x7E — volatile, resets on power cycle, ~45ms); mutually exclusive with --addr
        #[usage(long)]
        global: bool,
        /// Lock the sector/block containing this address (0x36 — volatile, resets on power cycle); mutually exclusive with --global
        #[usage(long, )]
        addr: Option<Hex<u32>>,
    },

    /// Unlock flash blocks (Winbond individual block unlock, 0x39/0x98)
    BlockUnlock {
        /// Unlock all blocks globally (0x98 — volatile, resets on power cycle, ~45ms); mutually exclusive with --addr
        #[usage(long)]
        global: bool,
        /// Unlock the sector/block containing this address (0x39 — volatile, resets on power cycle); mutually exclusive with --global
        #[usage(long, )]
        addr: Option<Hex<u32>>,
    },

    /// Parse a layout file and list regions (no hardware required)
    Regions {
        #[usage(short, long, value_hint = usage::ValueHint::FilePath)]
        file: PathBuf,
    },

    /// Read OTP security registers (Winbond/GigaDevice, opcode 0x48)
    Otp {
        #[usage(subcommand)]
        action: OtpCmd,
    },

    /// Parallel NOR flash (x16) on the xPort or Mach1 (EXPIO protocol)
    Pnor {
        #[usage(subcommand)]
        action: PnorCmd,
    },

    /// Print or install the shell completion script
    Completions {
        /// Shell to generate completions for
        #[usage(value_enum)]
        shell: Shell,
        /// Install the script where the shell looks for it instead of printing it
        #[usage(long, effect = "write")]
        install: bool,
        /// Replace an existing file at the install path that flashcat did not write
        #[usage(long, requires = "--install", effect = "write")]
        force: bool,
    },
}

#[derive(Clone, Copy, usage::ValueEnum)]
#[usage(rename_all = "snake_case")]
enum Shell {
    Bash,
    Elvish,
    Fish,
    Nu,
    Zsh,
}

impl From<Shell> for usage::complete::Shell {
    fn from(shell: Shell) -> Self {
        match shell {
            Shell::Bash => Self::Bash,
            Shell::Elvish => Self::Elvish,
            Shell::Fish => Self::Fish,
            Shell::Nu => Self::Nu,
            Shell::Zsh => Self::Zsh,
        }
    }
}

/// Print the completion script, or install it without touching any shell rc file.
fn cmd_completions(shell: Shell, install: bool, force: bool) -> Result<()> {
    use usage::install::{Env, Loading, OnForeign, Wrote};

    let shell = shell.into();
    if !install {
        println!("{}", Cli::completion_script(shell).trim_end());
        return Ok(());
    }
    let on_foreign = if force { OnForeign::Overwrite } else { OnForeign::Refuse };
    let done = Cli::install_completion(shell, &Env::from_process(), on_foreign).map_err(|e| {
        match e {
            usage::install::Error::Foreign { .. } => {
                anyhow::anyhow!("{e}\n\nPass --force to replace it, or redirect the script yourself.")
            }
            e => anyhow::Error::new(e),
        }
    })?;
    eprintln!("installed to {}", done.plan.path.display());
    if done.wrote == Wrote::Unchanged {
        eprintln!("already up to date");
    }
    if let Some(line) = done.plan.loading.instruction() {
        let file = match &done.plan.loading {
            Loading::Manual { file, .. } => file.as_str(),
            _ => "your shell's startup file",
        };
        eprintln!("\nadd this to {file}, once:\n\n{line}\n");
    }
    if let Some(note) = done.plan.note {
        eprintln!("note: {note}");
    }
    Ok(())
}

#[derive(usage::Subcommands)]
enum PnorCmd {
    /// Identify the attached parallel NOR chip (manufacturer + device ID)
    Detect,
    /// Read parallel NOR to a file
    Read {
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: PathBuf,
        #[usage(long, default = "0")]
        offset: Hex<u32>,
        /// Bytes to read (default: full chip size if known)
        #[usage(long, )]
        length: Option<Hex<u32>>,
    },
    /// Full-chip erase (AMD command set)
    Erase,
    /// Write a file to parallel NOR (chip must be erased first; --erase to do both)
    Write {
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: PathBuf,
        #[usage(long, default = "0")]
        offset: Hex<u32>,
        /// Chip-erase before writing
        #[usage(long)]
        erase: bool,
        /// Read back and verify after writing
        #[usage(long)]
        verify: bool,
    },
}

#[derive(usage::Subcommands)]
enum OtpCmd {
    /// Dump security register(s) to stdout (hexdump) or a file
    Read {
        /// Register number to read (1-based); omit to read all
        #[usage(long, )]
        reg: Option<Hex<u32>>,
        /// Write raw register bytes to this file instead of hexdumping
        #[usage(short, long, value_name = "FILE", value_hint = usage::ValueHint::FilePath)]
        file: Option<PathBuf>,
    },
    /// Show OTP lock bits LB1-3 (whether each register is permanently locked)
    LockStatus,
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

    if let Cmd::Completions { shell, install, force } = cli.cmd {
        return cmd_completions(shell, install, force);
    }

    usb::set_selector(cli.programmer.as_deref().map(usb::DeviceSelector::parse));
    fpga::set_mach1_quad(matches!(&cli.cmd, Cmd::Read { quad: true, .. }));

    let vc = match cli.voltage.as_str() {
        "auto"        => VoltageChoice::Auto,
        "1v8" | "1.8" => VoltageChoice::Explicit(Voltage::V1_8),
        "3v3" | "3.3" => VoltageChoice::Explicit(Voltage::V3_3),
        "5v"  | "5.0" => VoltageChoice::Explicit(Voltage::V5_0),
        v => bail!("unknown voltage '{v}' — use auto, 1v8, 3v3, or 5v"),
    };

    let speed = SpiSpeed(cli.mhz.0);

    match &cli.cmd {
        Cmd::Check => cmd::cmd_check().await,
        Cmd::Devices => cmd::cmd_devices().await,
        Cmd::Watch => cmd::cmd_watch(vc, speed).await,
        Cmd::Detect => cmd::cmd_detect(vc, speed).await,
        Cmd::Read { file, offset, length, quad, legacy_read, layout, region, read_repeated } => {
            let passes = match read_repeated {
                Some(n) if *n < 3 || *n > 100 => bail!("--read-repeated must be between 3 and 100"),
                Some(n) => *n,
                None => 1,
            };
            cmd::cmd_read(cmd::ReadOpts {
                vc, speed,
                file: file.clone(),
                offset: offset.0,
                length: length.map(|l| l.0),
                quad: *quad,
                legacy_read: *legacy_read,
                layout: layout.clone(),
                region: region.clone(),
                passes,
            }).await
        }
        Cmd::Write { file, offset, erase, verify, smart, layout, region } => {
            cmd::cmd_write(cmd::WriteOpts {
                vc, speed,
                file: file.clone(),
                offset: offset.0,
                erase: *erase,
                verify: *verify,
                smart: *smart,
                layout: layout.clone(),
                region: region.clone(),
            }).await
        }
        Cmd::Memtest { offset, length, layout, region, backup, force, thorough, seed, report } => {
            cmd::cmd_memtest(cmd::MemtestOpts {
                vc, speed,
                offset: offset.map(|o| o.0),
                length: length.map(|l| l.0),
                layout: layout.clone(),
                region: region.clone(),
                backup: backup.clone(),
                force: *force,
                thorough: *thorough,
                seed: seed.map(|s| s.0),
                report: report.clone(),
            }).await
        }
        Cmd::Sfdp => cmd::cmd_sfdp(vc, speed).await,
        Cmd::Erase { offset, length, layout, region } => {
            cmd::cmd_erase(vc, speed, offset.map(|o| o.0), length.map(|l| l.0), layout.clone(), region.clone()).await
        }
        Cmd::Compare { file, offset, length, layout, region } => {
            cmd::cmd_compare(cmd::CompareOpts {
                vc, speed,
                file: file.clone(),
                offset: offset.0,
                length: length.map(|l| l.0),
                layout: layout.clone(),
                region: region.clone(),
            }).await
        }
        Cmd::Fmap { scan_limit, file } => cmd::cmd_fmap(vc, speed, scan_limit.0, file.clone()).await,
        Cmd::Uid => cmd::cmd_uid(vc, speed).await,
        Cmd::Status => cmd::cmd_status(vc, speed).await,
        Cmd::Protect => cmd::cmd_protect(vc, speed).await,
        Cmd::Unprotect => cmd::cmd_unprotect(vc, speed).await,
        Cmd::BlockLock { global, addr } => cmd::cmd_block_lock(vc, speed, *global, addr.map(|a| a.0)).await,
        Cmd::BlockUnlock { global, addr } => cmd::cmd_block_unlock(vc, speed, *global, addr.map(|a| a.0)).await,
        Cmd::Regions { file } => cmd::cmd_regions(file.clone()).await,
        Cmd::Otp { action } => match action {
            OtpCmd::Read { reg, file } => {
                cmd::cmd_otp_read(vc, speed, reg.map(|r| r.0 as u8), file.clone()).await
            }
            OtpCmd::LockStatus => cmd::cmd_otp_lock_status(vc, speed).await,
        },
        Cmd::Pnor { action } => match action {
            PnorCmd::Detect => cmd::cmd_pnor_detect(vc).await,
            PnorCmd::Read { file, offset, length } => {
                cmd::cmd_pnor_read(vc, file.clone(), offset.0, length.map(|l| l.0)).await
            }
            PnorCmd::Erase => cmd::cmd_pnor_erase(vc).await,
            PnorCmd::Write { file, offset, erase, verify } => {
                cmd::cmd_pnor_write(vc, file.clone(), offset.0, *erase, *verify).await
            }
        },
        Cmd::Completions { .. } => unreachable!("handled before device setup"),
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
    let result: Result<()> = async {
        fpga::load(&dev, voltage).await?;
        fpga::set_vcc(&dev, voltage).await?;
        spi::init(&dev, speed).await?;
        Ok(())
    }.await;
    if let Err(e) = result {
        power_down_and_vcc_off(&dev).await;
        return Err(e);
    }
    Ok(dev)
}

pub(crate) async fn power_down_and_vcc_off(dev: &usb::UsbDevice) {
    if let Err(e) = spi::deep_power_down(dev).await {
        tracing::debug!("deep_power_down before vcc_off: {e}");
    }
    if let Err(e) = fpga::vcc_off(dev).await {
        tracing::debug!("vcc_off: {e}");
    }
}

/// Run `body` against the open device, racing it against Ctrl-C, and ensure
/// `power_down_and_vcc_off(dev)` runs on every exit path — clean completion,
/// body error, or user interrupt. Use this from every cmd_* that holds a
/// `UsbDevice`; it replaces the old `let result = (async {...}).await;
/// power_down_and_vcc_off(&dev).await; result` boilerplate.
///
/// SIGINT is the common "I want VCC off now" trigger. Without this wrapper a
/// Ctrl-C during a long write/erase aborts the task without sending DPD or
/// LogicOff, leaving the chip powered.
pub(crate) async fn with_cleanup<F, T>(dev: &usb::UsbDevice, body: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::pin!(body);

    let result = tokio::select! {
        biased;
        r = &mut body => r,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\ninterrupted — cutting VCC");
            Err(anyhow::anyhow!("interrupted by user"))
        }
    };

    power_down_and_vcc_off(dev).await;
    result
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
            match spi::detect(&dev, voltage).await {
                Ok(Some(chip)) => Ok((dev, chip, voltage)),
                Ok(None) => {
                    power_down_and_vcc_off(&dev).await;
                    anyhow::bail!("no chip detected")
                }
                Err(e) => {
                    power_down_and_vcc_off(&dev).await;
                    Err(e)
                }
            }
        }
    }
}

