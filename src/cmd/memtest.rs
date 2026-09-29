use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::bios::layout;
use crate::chip::ResolvedChip;
use crate::memtest::{self, FaultMap, Pattern, Report, Step};
use crate::progress::Progress;
use crate::spi::{self, SpiSpeed};
use crate::usb::UsbDevice;
use crate::{prepare, with_cleanup, VoltageChoice};

const SLOW_FACTOR: u32 = 3;
const SLOW_FLOOR: Duration = Duration::from_millis(20);
const MAX_LISTED: usize = 32;

pub struct MemtestOpts {
    pub vc: VoltageChoice,
    pub speed: SpiSpeed,
    pub offset: Option<u32>,
    pub length: Option<u32>,
    pub layout: Option<PathBuf>,
    pub region: Option<String>,
    pub backup: Option<PathBuf>,
    pub force: bool,
    pub thorough: bool,
    pub seed: Option<u64>,
    pub report: Option<PathBuf>,
}

/// Erase granularity used for the test and for timing samples.
struct EraseUnit {
    size: u32,
    opcode: u8,
}

pub async fn cmd_memtest(opts: MemtestOpts) -> Result<()> {
    let (dev, chip, _voltage) = prepare(opts.vc, opts.speed).await?;
    with_cleanup(&dev, run(&dev, &chip, &opts)).await
}

async fn run(dev: &UsbDevice, chip: &ResolvedChip, opts: &MemtestOpts) -> Result<()> {
    let (offset, length) = resolve_range(dev, chip, opts).await?;

    let wp = spi::read_wp_status(dev, chip).await?;
    if wp.block_lock_mode {
        bail!("chip is in block-lock mode — run `flashcat block-unlock --global` first");
    }
    if wp.range != "none" {
        bail!("chip is write-protected ({}) — run `flashcat unprotect` first", wp.range);
    }

    let unit = erase_unit(chip, offset, length)?;
    let seed = opts.seed.unwrap_or_else(time_seed);
    let steps = memtest::plan(opts.thorough, seed);

    let original = spi::read(dev, chip, offset, length, false).await?;
    if let Some(path) = &opts.backup {
        std::fs::write(path, &original)
            .with_context(|| format!("failed to write backup {}", path.display()))?;
        println!(
            "Backup: {} ({} bytes) — if interrupted, restore with `flashcat write -f {} --offset {offset:#x}`",
            path.display(), original.len(), path.display()
        );
    } else if !opts.force && original.iter().any(|&b| b != 0xFF) {
        bail!(
            "range {offset:#010x}..{:#010x} is not blank — memtest destroys its contents.\n\
             Pass --backup FILE to save and restore them, or --force to wipe them.",
            offset + length
        );
    }

    println!(
        "memtest: {} {offset:#010x}..{:#010x} ({} KB), {} passes, {} KB erase unit",
        chip.name, offset + length, length / 1024, steps.len(), unit.size / 1024
    );
    if opts.thorough {
        println!("Seed: {seed:#x} (reproduce with --seed {seed:#x})");
    }

    let mut t = Tester {
        dev, chip, offset, length, unit,
        faults: FaultMap::default(),
        aliases: vec![],
        erase_times: vec![],
        program_times: vec![],
        erase_cycles: 0,
    };
    let result = t.run_steps(&steps).await;

    let finish = match &opts.backup {
        Some(_) => t.restore(&original).await,
        None if result.is_ok() => t.erase_all("Erasing").await.map(|_| println!("Range left erased")),
        None => Ok(()),
    };
    if let (Err(_), Err(e)) = (&result, &finish) {
        eprintln!("restore failed: {e:#}");
    }
    result?;

    let report = t.report(&steps, opts.thorough.then_some(seed));
    print_report(&report, chip);
    if let Some(path) = &opts.report {
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(path, json).with_context(|| format!("failed to write report {}", path.display()))?;
        println!("Report: {}", path.display());
    }
    finish?;
    if !report.passed {
        bail!(
            "memtest FAILED: {} bad sector(s), {} alias pattern(s)",
            report.bad_sectors.len(), report.aliases.len()
        );
    }
    Ok(())
}

async fn resolve_range(dev: &UsbDevice, chip: &ResolvedChip, opts: &MemtestOpts) -> Result<(u32, u32)> {
    let (off, len) =
        match layout::resolve_region_flags(opts.region.as_deref(), opts.layout.as_deref(), chip, dev, opts.speed).await? {
            Some((off, len)) => (off, Some(len)),
            None => (opts.offset.unwrap_or(0), opts.length),
        };
    if off >= chip.size_bytes {
        bail!("offset {off:#x} exceeds chip size {:#x}", chip.size_bytes);
    }
    let max_len = chip.size_bytes - off;
    let len = match len {
        Some(0) => bail!("length must be > 0"),
        Some(l) if l > max_len => bail!("length {l:#x} exceeds available space {max_len:#x} at offset {off:#x}"),
        Some(l) => l,
        None => max_len,
    };
    let sector = chip.erase_size;
    if !off.is_multiple_of(sector) || !len.is_multiple_of(sector) {
        bail!("offset and length must be multiples of the {sector}-byte erase sector (memtest never erases outside the range)");
    }
    Ok((off, len))
}

/// 64 KB blocks when the range allows it (3-byte parts only, matching
/// write_smart), otherwise the chip's smallest sector.
fn erase_unit(chip: &ResolvedChip, offset: u32, length: u32) -> Result<EraseUnit> {
    let block = chip.erase_types.iter().find(|e| e.size_bytes == 65536);
    if let Some(b) = block
        && chip.addr_bytes == 3
        && offset.is_multiple_of(65536)
        && length.is_multiple_of(65536)
    {
        return Ok(EraseUnit { size: 65536, opcode: b.opcode });
    }
    let sector = chip.erase_types.iter()
        .find(|e| e.size_bytes == chip.erase_size)
        .ok_or_else(|| anyhow::anyhow!("no erase type for {}B in chip {}", chip.erase_size, chip.name))?;
    Ok(EraseUnit { size: sector.size_bytes, opcode: sector.opcode })
}

fn time_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED)
}

struct Tester<'a> {
    dev: &'a UsbDevice,
    chip: &'a ResolvedChip,
    offset: u32,
    length: u32,
    unit: EraseUnit,
    faults: FaultMap,
    aliases: Vec<memtest::Alias>,
    erase_times: Vec<(u32, Duration)>,
    program_times: Vec<(u32, Duration)>,
    erase_cycles: u32,
}

impl Tester<'_> {
    fn units(&self) -> impl Iterator<Item = u32> + use<> {
        (self.offset..self.offset + self.length).step_by(self.unit.size as usize)
    }

    async fn run_steps(&mut self, steps: &[Step]) -> Result<()> {
        for (i, step) in steps.iter().enumerate() {
            println!("[{}/{}] {}", i + 1, steps.len(), step.pattern.label());
            if step.erase {
                self.erase_all("Erasing").await?;
            }
            if step.pattern.programs() {
                self.program(step.pattern).await?;
            }
            let bad = self.verify(step.pattern).await?;
            if bad == 0 {
                println!("      ok");
            } else {
                println!("      {bad} sector(s) failed");
            }
        }
        Ok(())
    }

    async fn erase_all(&mut self, label: &str) -> Result<()> {
        let mut pb = Progress::new(label, self.length as u64);
        for addr in self.units() {
            let t0 = Instant::now();
            spi::erase_unit_opcode(self.dev, self.chip, addr, self.unit.opcode, self.unit.size > 4096)
                .await
                .with_context(|| format!("erase at {addr:#010x} did not complete — sector failed or connection lost"))?;
            self.erase_times.push((addr, t0.elapsed()));
            pb.inc(self.unit.size as u64);
        }
        pb.finish();
        self.erase_cycles += 1;
        Ok(())
    }

    async fn program(&mut self, pattern: Pattern) -> Result<()> {
        let mut pb = Progress::new("Writing", self.length as u64);
        let mut buf = vec![0u8; self.unit.size as usize];
        for addr in self.units() {
            pattern.fill(addr, &mut buf);
            let t0 = Instant::now();
            spi::write_block(self.dev, self.chip, addr, &buf)
                .await
                .with_context(|| format!("program at {addr:#010x} did not complete — sector failed or connection lost"))?;
            self.program_times.push((addr, t0.elapsed()));
            pb.inc(self.unit.size as u64);
        }
        pb.finish();
        Ok(())
    }

    /// Read back and compare per sector; mismatches get one confirm read to
    /// separate real cell faults from flaky transfers. Returns failed sectors.
    async fn verify(&mut self, pattern: Pattern) -> Result<usize> {
        let actual = spi::read(self.dev, self.chip, self.offset, self.length, false).await?;
        let sector = self.chip.erase_size;
        let pass = pattern.short();
        let mut expected = vec![0u8; sector as usize];
        let mut bad = 0;
        for (i, got) in actual.chunks(sector as usize).enumerate() {
            let addr = self.offset + i as u32 * sector;
            pattern.fill(addr, &mut expected);
            if got == expected.as_slice() {
                continue;
            }
            let confirm = spi::read::try_read_block(self.dev, self.chip, addr, sector, false).await?;
            let errs = memtest::bit_errors(&expected, &confirm);
            if errs.is_clean() {
                self.faults.record_unstable(addr, &pass);
            } else {
                self.faults.record(addr, errs, &pass);
                bad += 1;
            }
        }
        if pattern == Pattern::Address {
            self.aliases = memtest::find_aliases(self.offset, &actual);
        }
        Ok(bad)
    }

    async fn restore(&mut self, original: &[u8]) -> Result<()> {
        println!("Restoring backup");
        self.erase_all("Erasing").await?;
        spi::write(self.dev, self.chip, self.offset, original).await?;
        let back = spi::read(self.dev, self.chip, self.offset, self.length, false).await?;
        let errs = memtest::bit_errors(original, &back);
        if let Some(i) = errs.first {
            bail!(
                "restore verify FAILED at {:#010x} ({} bit(s) off) — backup file is intact, rewrite it",
                self.offset + i as u32, errs.stuck0 + errs.stuck1
            );
        }
        println!("Restored and verified {} bytes", original.len());
        Ok(())
    }

    fn report(&self, steps: &[Step], seed: Option<u64>) -> Report {
        let (erase_median, slow_erase) = memtest::slow_outliers(&self.erase_times, SLOW_FACTOR, SLOW_FLOOR);
        let (program_median, slow_program) = memtest::slow_outliers(&self.program_times, SLOW_FACTOR, SLOW_FLOOR);
        let bad_sectors: Vec<_> = self.faults.sectors.values().cloned().collect();
        Report {
            chip: self.chip.name.clone(),
            offset: self.offset,
            length: self.length,
            sector_size: self.chip.erase_size,
            erase_unit: self.unit.size,
            passes: steps.iter().map(|s| s.pattern.short()).collect(),
            seed,
            erase_cycles: self.erase_cycles,
            passed: bad_sectors.is_empty() && self.aliases.is_empty(),
            bad_sectors,
            unstable_sectors: self.faults.unstable.keys().copied().collect(),
            aliases: self.aliases.clone(),
            erase_median_ms: memtest::ms(erase_median),
            program_median_ms: memtest::ms(program_median),
            slow_erase,
            slow_program,
        }
    }
}

fn print_report(r: &Report, chip: &ResolvedChip) {
    println!();
    println!(
        "Timing:  erase {:.1} ms / program {:.1} ms median per {} KB unit",
        r.erase_median_ms, r.program_median_ms, r.erase_unit / 1024
    );
    println!("Wear:    {} erase cycle(s) per sector", r.erase_cycles);

    for a in &r.aliases {
        println!("ALIAS:   {}", memtest::describe_alias(a, chip.size_bytes));
    }
    if !r.bad_sectors.is_empty() {
        println!("Bad sectors ({}):", r.bad_sectors.len());
        for f in r.bad_sectors.iter().take(MAX_LISTED) {
            println!(
                "  {:#010x}  stuck0={:<6} stuck1={:<6} first={:#010x}  [{}]",
                f.addr, f.stuck0, f.stuck1, f.first_bad, f.passes.join(",")
            );
        }
        if r.bad_sectors.len() > MAX_LISTED {
            println!("  ... ({} more)", r.bad_sectors.len() - MAX_LISTED);
        }
    }
    if !r.unstable_sectors.is_empty() {
        println!(
            "Unstable reads ({}) — mismatched once, matched on re-read (check wiring/clock):",
            r.unstable_sectors.len()
        );
        for a in r.unstable_sectors.iter().take(MAX_LISTED) {
            println!("  {a:#010x}");
        }
    }
    for (label, slow, median) in [
        ("erase", &r.slow_erase, r.erase_median_ms),
        ("program", &r.slow_program, r.program_median_ms),
    ] {
        if slow.is_empty() {
            continue;
        }
        println!("Slow {label} ({}) — >{SLOW_FACTOR}× median {median:.1} ms, early wear sign:", slow.len());
        for s in slow.iter().take(MAX_LISTED) {
            println!("  {:#010x}  {:.1} ms", s.addr, s.ms);
        }
    }
    println!("Result:  {}", if r.passed { "PASS" } else { "FAIL" });
}
