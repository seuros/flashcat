/// Intel Flash Descriptor (IFD) parser.
///
/// Supports IFD v1 (ICH8..PCH Series 9+). The FLVALSIG 0x0FF0A55A identifies
/// the descriptor; region table offset is encoded in FLMAP0.
pub const FLVALSIG: u32 = 0x0FF0_A55A;

/// Standard region names by index (IFD v1 / v2).
const REGION_NAMES: &[&str] = &[
    "FD",           // 0 — Flash Descriptor
    "BIOS",         // 1
    "ME",           // 2 — Management Engine
    "GbE",          // 3 — Gigabit Ethernet
    "PDR",          // 4 — Platform Data
    "DevExp1",      // 5 — Device Expansion 1 (Series 8+)
    "BIOS2",        // 6
    "NetArbT",      // 7
    "EC",           // 8 — Embedded Controller
    "DevExp2",      // 9
    "IE",           // 10 — Innovation Engine
    "10GbEA",       // 11
    "10GbEB",       // 12
    "Reserved13",   // 13
    "Reserved14",   // 14
    "PTT",          // 15
];

#[derive(Debug, Clone)]
pub struct IfdRegion {
    pub name: String,
    #[allow(dead_code)]
    pub index: usize,
    pub start: u32,
    pub end: u32,   // inclusive
    pub present: bool,
}

impl IfdRegion {
    pub fn length(&self) -> u32 {
        if self.present { self.end - self.start + 1 } else { 0 }
    }
}

#[derive(Debug)]
pub struct IfdInfo {
    pub descriptor_offset: usize,
    #[allow(dead_code)]
    pub nr_regions: usize,
    pub regions: Vec<IfdRegion>,
}

/// Scan `data` for FLVALSIG and parse the IFD.
pub fn scan_ifd(data: &[u8]) -> Option<IfdInfo> {
    // Scan for 0x0FF0A55A in 4-byte aligned positions.
    let pos = (0..data.len().saturating_sub(3))
        .step_by(4)
        .find(|&i| u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) == FLVALSIG)?;

    parse_ifd(data, pos)
}

fn parse_ifd(data: &[u8], sig_offset: usize) -> Option<IfdInfo> {
    if sig_offset + 16 > data.len() {
        return None;
    }

    let flmap0 = u32::from_le_bytes(data[sig_offset + 4..sig_offset + 8].try_into().ok()?);

    // FRBA [23:16] of FLMAP0, actual byte offset from flash image start = ((flmap0 >> 16) & 0xFF) << 4
    let frba = (((flmap0 >> 16) & 0xFF) as usize) << 4;

    // NR [26:24] of FLMAP0 — number of regions minus one? Actually number of region entries.
    // Intel uses NR as the count of defined regions (not minus-one). Some versions use it
    // differently; we'll read up to 16 entries and check validity.
    let nr_hint = (((flmap0 >> 24) & 0x7) as usize) + 1;
    let nr_regions = nr_hint.max(5).min(REGION_NAMES.len()); // always try at least 5

    if frba + nr_regions * 4 > data.len() {
        return None;
    }

    let mut regions = Vec::new();
    for i in 0..nr_regions {
        let entry_offset = frba + i * 4;
        if entry_offset + 4 > data.len() { break; }
        let flreg = u32::from_le_bytes(data[entry_offset..entry_offset + 4].try_into().ok()?);

        let base  = (flreg & 0x0FFF) as u32;        // bits[11:0]
        let limit = ((flreg >> 16) & 0x0FFF) as u32; // bits[27:16]

        let present = limit >= base && !(base == 0 && limit == 0 && i > 0);
        let name = REGION_NAMES.get(i).unwrap_or(&"Unknown").to_string();

        regions.push(IfdRegion {
            name,
            index: i,
            start: base << 12,
            end: (limit << 12) | 0xFFF,
            present,
        });
    }

    Some(IfdInfo { descriptor_offset: sig_offset, nr_regions, regions })
}

pub fn print_ifd(info: &IfdInfo, flash_size: Option<u32>) {
    println!("Intel Flash Descriptor (IFD)");
    println!("  signature at : {:#010x}", info.descriptor_offset);
    if let Some(sz) = flash_size {
        println!("  flash size   : {:#010x} ({} MB)", sz, sz / (1024 * 1024));
    }
    println!();
    println!("{:<12} {:<12} {:<12} {:<10}  {}", "REGION", "START", "END", "SIZE", "STATUS");
    println!("{}", "-".repeat(64));
    for r in &info.regions {
        if r.present {
            println!(
                "  {:<12} {:#010x}  {:#010x}  {:<10}  present",
                r.name, r.start, r.end, r.length(),
            );
        } else {
            println!("  {:<12} —                             absent", r.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_no_sig() {
        assert!(scan_ifd(&[0xFF; 256]).is_none());
    }

    #[test]
    fn test_scan_finds_sig() {
        // Minimal descriptor at offset 0 with 1 present region
        let mut data = vec![0u8; 0x100];
        // FLVALSIG at 0
        data[0..4].copy_from_slice(&FLVALSIG.to_le_bytes());
        // FLMAP0: FRBA=0x04 (offset 0x40), NR=0 (1 region)
        // ((0x04 << 16) | 0) = 0x00040000
        data[4..8].copy_from_slice(&0x00040000u32.to_le_bytes());
        // FLREG0 at 0x40: base=0, limit=0 → present
        // 0x00000000
        data[0x40..0x44].copy_from_slice(&0x00000000u32.to_le_bytes());

        let info = scan_ifd(&data).unwrap();
        assert_eq!(info.descriptor_offset, 0);
        assert!(info.regions[0].present);
    }
}
