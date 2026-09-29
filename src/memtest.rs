//! Destructive SPI NOR self-test: pattern generation and fault analysis.
//! Hardware orchestration lives in `cmd::memtest`.

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

const ADDR_SALT: u32 = 0x5A3C_96E1;

/// Minimum words sharing one XOR mask before it is reported as aliasing.
/// Corrupted words decode to effectively random addresses, so only a
/// consistent mask across many words is an address fault.
const ALIAS_MIN_WORDS: u32 = 16;

/// Timing outliers need a meaningful baseline.
const OUTLIER_MIN_SAMPLES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    /// Erased state: every byte reads 0xFF.
    Blank,
    Fill(u8),
    /// Each 32-bit LE word holds `encode_addr(address)`.
    Address,
    /// Position-dependent pseudo-random words (stateless, chunkable).
    Random(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub pattern: Pattern,
    /// Erase the range before programming this pattern.
    pub erase: bool,
}

impl Pattern {
    pub fn label(&self) -> String {
        match self {
            Pattern::Blank => "erase + blank check (0xFF)".into(),
            Pattern::Fill(b) => format!("fill {b:#04x}"),
            Pattern::Address => "address-in-data (aliasing / capacity)".into(),
            Pattern::Random(seed) => format!("pseudo-random (seed {seed:#x})"),
        }
    }

    pub fn short(&self) -> String {
        match self {
            Pattern::Blank => "blank".into(),
            Pattern::Fill(b) => format!("{b:#04x}"),
            Pattern::Address => "address".into(),
            Pattern::Random(_) => "random".into(),
        }
    }

    pub fn programs(&self) -> bool {
        !matches!(self, Pattern::Blank)
    }

    /// Fill `buf` with the expected content of flash starting at `base`.
    /// `base` and `buf.len()` must be 4-byte aligned for word patterns.
    pub fn fill(&self, base: u32, buf: &mut [u8]) {
        match *self {
            Pattern::Blank => buf.fill(0xFF),
            Pattern::Fill(b) => buf.fill(b),
            Pattern::Address => fill_words(base, buf, encode_addr),
            Pattern::Random(seed) => fill_words(base, buf, |a| splitmix64(seed ^ a as u64) as u32),
        }
    }
}

fn fill_words(base: u32, buf: &mut [u8], word: impl Fn(u32) -> u32) {
    debug_assert!(base.is_multiple_of(4) && buf.len().is_multiple_of(4));
    for (i, chunk) in buf.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&word(base.wrapping_add(i as u32 * 4)).to_le_bytes());
    }
}

/// Invertible non-linear mix (murmur3 fmix32). Unlike a plain XOR, a flipped
/// data bit does not decode to a neighbouring address, so stuck bits can't be
/// mistaken for address-line aliasing.
pub fn encode_addr(a: u32) -> u32 {
    let mut h = a ^ ADDR_SALT;
    h ^= h >> 16;
    h = h.wrapping_mul(M1);
    h ^= h >> 13;
    h = h.wrapping_mul(M2);
    h ^ (h >> 16)
}

pub fn decode_addr(w: u32) -> u32 {
    let mut h = w;
    h ^= h >> 16;
    h = h.wrapping_mul(inv_mod32(M2));
    h ^= (h >> 13) ^ (h >> 26);
    h = h.wrapping_mul(inv_mod32(M1));
    h ^= h >> 16;
    h ^ ADDR_SALT
}

const M1: u32 = 0x85EB_CA6B;
const M2: u32 = 0xC2B2_AE35;

/// Multiplicative inverse of an odd `a` modulo 2^32 (Newton iteration).
const fn inv_mod32(a: u32) -> u32 {
    let mut x = a;
    let mut i = 0;
    while i < 5 {
        x = x.wrapping_mul(2u32.wrapping_sub(a.wrapping_mul(x)));
        i += 1;
    }
    x
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Quick plan: blank, zeros, checkerboard pair, address. Thorough adds random.
pub fn plan(thorough: bool, seed: u64) -> Vec<Step> {
    let mut steps = vec![
        Step { pattern: Pattern::Blank, erase: true },
        // Programs straight onto the freshly verified erased state.
        Step { pattern: Pattern::Fill(0x00), erase: false },
        Step { pattern: Pattern::Fill(0x55), erase: true },
        Step { pattern: Pattern::Fill(0xAA), erase: true },
        Step { pattern: Pattern::Address, erase: true },
    ];
    if thorough {
        steps.push(Step { pattern: Pattern::Random(seed), erase: true });
    }
    steps
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BitErrors {
    /// Expected 1, read 0: cell won't erase / stuck at 0.
    pub stuck0: u32,
    /// Expected 0, read 1: cell won't program / stuck at 1.
    pub stuck1: u32,
    /// Offset of the first differing byte.
    pub first: Option<usize>,
}

impl BitErrors {
    pub fn is_clean(&self) -> bool {
        self.first.is_none()
    }
}

pub fn bit_errors(expected: &[u8], actual: &[u8]) -> BitErrors {
    let mut e = BitErrors::default();
    for (i, (&x, &y)) in expected.iter().zip(actual).enumerate() {
        if x == y {
            continue;
        }
        e.stuck0 += (x & !y).count_ones();
        e.stuck1 += (!x & y).count_ones();
        e.first.get_or_insert(i);
    }
    if expected.len() != actual.len() {
        e.first.get_or_insert(expected.len().min(actual.len()));
    }
    e
}

/// Per-sector fault accumulated across all passes.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SectorFault {
    pub addr: u32,
    pub stuck0: u32,
    pub stuck1: u32,
    pub first_bad: u32,
    pub passes: Vec<String>,
}

#[derive(Default)]
pub struct FaultMap {
    pub sectors: BTreeMap<u32, SectorFault>,
    /// Sectors whose first readback mismatched but the confirm read matched.
    pub unstable: BTreeMap<u32, Vec<String>>,
}

impl FaultMap {
    pub fn record(&mut self, addr: u32, errs: BitErrors, pass: &str) {
        let f = self.sectors.entry(addr).or_insert_with(|| SectorFault {
            addr,
            first_bad: addr + errs.first.unwrap_or(0) as u32,
            ..Default::default()
        });
        f.stuck0 += errs.stuck0;
        f.stuck1 += errs.stuck1;
        f.passes.push(pass.to_string());
    }

    pub fn record_unstable(&mut self, addr: u32, pass: &str) {
        self.unstable.entry(addr).or_default().push(pass.to_string());
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Alias {
    /// `read_addr ^ written_addr` shared by all affected words.
    pub mask: u32,
    pub words: u32,
}

/// Find XOR masks linking where address-pattern words were read back to where
/// they were written. A consistent mask across many words means two addresses
/// hit the same cell (address line fault or fake capacity).
pub fn find_aliases(base: u32, actual: &[u8]) -> Vec<Alias> {
    let len = actual.len() as u64;
    let mut masks: BTreeMap<u32, u32> = BTreeMap::new();
    for (i, w) in actual.chunks_exact(4).enumerate() {
        let a = base + i as u32 * 4;
        let d = decode_addr(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
        if d == a || !d.is_multiple_of(4) || d < base || (d - base) as u64 >= len {
            continue;
        }
        *masks.entry(a ^ d).or_default() += 1;
    }
    let mut out: Vec<Alias> = masks
        .into_iter()
        .filter(|&(_, words)| words >= ALIAS_MIN_WORDS)
        .map(|(mask, words)| Alias { mask, words })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.words));
    out
}

/// Human diagnosis for an alias mask on a chip of `chip_size` bytes.
pub fn describe_alias(alias: &Alias, chip_size: u32) -> String {
    let m = alias.mask;
    if !m.is_power_of_two() {
        return format!("mask {m:#x}: multiple address bits collide ({} words)", alias.words);
    }
    let bit = m.trailing_zeros();
    if m.checked_mul(2) == Some(chip_size) {
        return format!(
            "address bit A{bit} ignored ({} words): real capacity is probably {} KB, not {} KB — likely relabeled/fake part",
            alias.words, m / 1024, chip_size / 1024
        );
    }
    format!("address bit A{bit} ignored or shorted ({} words)", alias.words)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Slow {
    pub addr: u32,
    pub ms: f64,
}

/// Units whose timing exceeds `factor` × median and the median by `floor`.
/// Returns the median and the outliers (slowest first).
pub fn slow_outliers(samples: &[(u32, Duration)], factor: u32, floor: Duration) -> (Duration, Vec<Slow>) {
    if samples.is_empty() {
        return (Duration::ZERO, vec![]);
    }
    let mut sorted: Vec<Duration> = samples.iter().map(|&(_, d)| d).collect();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    if samples.len() < OUTLIER_MIN_SAMPLES {
        return (median, vec![]);
    }
    let limit = (median * factor).max(median + floor);
    let mut slow: Vec<(u32, Duration)> = samples.iter().copied().filter(|&(_, d)| d > limit).collect();
    slow.sort_by_key(|s| std::cmp::Reverse(s.1));
    (median, slow.into_iter().map(|(addr, d)| Slow { addr, ms: ms(d) }).collect())
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[derive(Serialize)]
pub struct Report {
    pub chip: String,
    pub offset: u32,
    pub length: u32,
    pub sector_size: u32,
    pub erase_unit: u32,
    pub passes: Vec<String>,
    pub seed: Option<u64>,
    pub erase_cycles: u32,
    pub bad_sectors: Vec<SectorFault>,
    pub unstable_sectors: Vec<u32>,
    pub aliases: Vec<Alias>,
    pub erase_median_ms: f64,
    pub program_median_ms: f64,
    pub slow_erase: Vec<Slow>,
    pub slow_program: Vec<Slow>,
    pub passed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_pattern_decodes_to_its_address() {
        let mut buf = vec![0u8; 64];
        Pattern::Address.fill(0x1000, &mut buf);
        for (i, w) in buf.chunks_exact(4).enumerate() {
            let v = decode_addr(u32::from_le_bytes(w.try_into().unwrap()));
            assert_eq!(v, 0x1000 + i as u32 * 4);
        }
    }

    #[test]
    fn addr_encoding_round_trips() {
        for a in (0..u32::MAX).step_by(0x0001_3579).chain([0, 4, u32::MAX]) {
            assert_eq!(decode_addr(encode_addr(a)), a, "{a:#x}");
        }
        assert_eq!(M1.wrapping_mul(inv_mod32(M1)), 1);
        assert_eq!(M2.wrapping_mul(inv_mod32(M2)), 1);
    }

    #[test]
    fn random_pattern_is_chunk_independent() {
        let p = Pattern::Random(0xDEAD_BEEF);
        let mut whole = vec![0u8; 8192];
        p.fill(0x2000, &mut whole);
        let mut lo = vec![0u8; 4096];
        let mut hi = vec![0u8; 4096];
        p.fill(0x2000, &mut lo);
        p.fill(0x3000, &mut hi);
        assert_eq!(&whole[..4096], &lo[..]);
        assert_eq!(&whole[4096..], &hi[..]);

        let mut other = vec![0u8; 4096];
        Pattern::Random(0xDEAD_BEEE).fill(0x2000, &mut other);
        assert_ne!(lo, other);
    }

    #[test]
    fn plan_zero_fill_skips_erase_and_thorough_adds_random() {
        let quick = plan(false, 7);
        assert_eq!(quick.len(), 5);
        assert!(quick[0].erase && quick[0].pattern == Pattern::Blank);
        assert!(!quick[1].erase && quick[1].pattern == Pattern::Fill(0));
        let full = plan(true, 7);
        assert_eq!(full.last().unwrap().pattern, Pattern::Random(7));
    }

    #[test]
    fn bit_errors_split_by_direction() {
        // 0xFF -> 0xF0: four bits failed to stay 1 (stuck0).
        // 0x00 -> 0x03: two bits failed to program (stuck1).
        let e = bit_errors(&[0xFF, 0x11, 0x00], &[0xF0, 0x11, 0x03]);
        assert_eq!(e, BitErrors { stuck0: 4, stuck1: 2, first: Some(0) });
        assert!(bit_errors(&[1, 2, 3], &[1, 2, 3]).is_clean());
    }

    #[test]
    fn bit_errors_flags_short_read() {
        let e = bit_errors(&[1, 2, 3], &[1, 2]);
        assert_eq!(e.first, Some(2));
    }

    #[test]
    fn fault_map_accumulates_across_passes() {
        let mut m = FaultMap::default();
        m.record(0x3000, BitErrors { stuck0: 1, stuck1: 0, first: Some(0x10) }, "0x55");
        m.record(0x3000, BitErrors { stuck0: 2, stuck1: 1, first: Some(0x20) }, "0xaa");
        let f = &m.sectors[&0x3000];
        assert_eq!((f.stuck0, f.stuck1, f.first_bad), (3, 1, 0x3010));
        assert_eq!(f.passes, ["0x55", "0xaa"]);
    }

    /// Simulate a chip with `real` bytes of cells that ignores higher address
    /// bits, written sequentially with the address pattern over `claimed` bytes.
    fn wrapped_readback(real: usize, claimed: usize) -> Vec<u8> {
        let mut want = vec![0u8; claimed];
        Pattern::Address.fill(0, &mut want);
        let mut cells = vec![0xFFu8; real];
        for (i, &b) in want.iter().enumerate() {
            cells[i % real] = b;
        }
        (0..claimed).map(|i| cells[i % real]).collect()
    }

    #[test]
    fn fake_capacity_is_detected_as_top_bit_alias() {
        let (real, claimed) = (64 * 1024, 128 * 1024);
        let read = wrapped_readback(real, claimed);
        let aliases = find_aliases(0, &read);
        assert_eq!(aliases, [Alias { mask: real as u32, words: (real / 4) as u32 }]);
        let msg = describe_alias(&aliases[0], claimed as u32);
        assert!(msg.contains("A16") && msg.contains("64 KB"), "{msg}");
    }

    #[test]
    fn stuck_data_bit_in_one_sector_is_not_an_alias() {
        let mut read = vec![0u8; 64 * 1024];
        Pattern::Address.fill(0, &mut read);
        // Data bit 12 of every word in one 4 KB sector stuck at 0.
        for w in read[0x4000..0x5000].chunks_exact_mut(4) {
            w[1] &= !0x10;
        }
        // Same with a stuck-at-1 bit in the top byte of another sector.
        for w in read[0x9000..0xA000].chunks_exact_mut(4) {
            w[3] |= 0x40;
        }
        assert!(find_aliases(0, &read).is_empty());
    }

    #[test]
    fn clean_readback_has_no_aliases() {
        let mut read = vec![0u8; 16 * 1024];
        Pattern::Address.fill(0x8000, &mut read);
        assert!(find_aliases(0x8000, &read).is_empty());
    }

    #[test]
    fn slow_outliers_need_factor_and_floor() {
        let ms = Duration::from_millis;
        let mut s: Vec<(u32, Duration)> = (0..16).map(|i| (i * 0x1000, ms(40))).collect();
        s[3].1 = ms(200); // 5× median, +160 ms: outlier
        s[7].1 = ms(55); // under 3×: fine
        let (median, slow) = slow_outliers(&s, 3, ms(20));
        assert_eq!(median, ms(40));
        assert_eq!(slow, [Slow { addr: 0x3000, ms: 200.0 }]);

        // Tiny medians: 3× of 1 ms is noise, the floor keeps it quiet.
        let tiny: Vec<(u32, Duration)> = (0..16).map(|i| (i, ms(if i == 0 { 4 } else { 1 }))).collect();
        assert!(slow_outliers(&tiny, 3, ms(20)).1.is_empty());
    }

    #[test]
    fn slow_outliers_skip_small_samples() {
        let s = [(0, Duration::from_millis(1)), (1, Duration::from_secs(1))];
        assert!(slow_outliers(&s, 3, Duration::ZERO).1.is_empty());
    }
}
