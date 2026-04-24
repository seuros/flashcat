#![warn(clippy::all)]

/// AMD Platform Security Processor (PSP) / Embedded Firmware Structure (EFS) parser.
///
/// AMD firmware images contain an Embedded Firmware Structure identified by the
/// cookie 0x55AA55AA at one of a small set of well-known offsets. The EFS holds
/// pointers to the PSP directory and BIOS directory trees.

pub const EFS_COOKIE: u32 = 0x55AA_55AA;

/// Canonical offsets where the EFS may appear in a SPI flash image.
pub const EFS_SEARCH_OFFSETS: &[u32] = &[
    0x0002_0000,
    0x0006_0000,
    0x000A_0000,
    0x000E_0000,
    0x00FA_0000,
    0x00FE_0000,
];

// Directory cookie constants.
const PSP_L1_COOKIE: u32 = 0x5053_5024; // '$PSP'
const PSP_COMBO_COOKIE: u32 = 0x5053_5032; // '2PSP'
const BIOS_L1_COOKIE: u32 = 0x4448_4224; // '$BHD'
const BIOS_COMBO_COOKIE: u32 = 0x4448_4232; // '2BHD'

fn cookie_name(cookie: u32) -> Option<&'static str> {
    match cookie {
        PSP_L1_COOKIE => Some("$PSP"),
        PSP_COMBO_COOKIE => Some("2PSP"),
        BIOS_L1_COOKIE => Some("$BHD"),
        BIOS_COMBO_COOKIE => Some("2BHD"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct AmdDir {
    pub offset: u32,
    pub cookie: String,
    pub num_entries: u32,
}

#[derive(Debug)]
pub struct AmdPspInfo {
    pub efs_offset: u32,
    pub psp_dir: Option<AmdDir>,
    pub bios_dir: Option<AmdDir>,
}

/// Scan `data` for an AMD Embedded Firmware Structure.
///
/// Returns `None` if no valid EFS is found at any of the canonical offsets.
pub fn scan_amd_psp(data: &[u8]) -> Option<AmdPspInfo> {
    for &ofs in EFS_SEARCH_OFFSETS {
        if let Some(info) = try_parse_efs(data, ofs) {
            return Some(info);
        }
    }
    None
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Attempt to parse an EFS at `efs_offset` within `data`.
fn try_parse_efs(data: &[u8], efs_offset: u32) -> Option<AmdPspInfo> {
    let base = efs_offset as usize;

    // Need at least 0x2C bytes for the EFS fields we use.
    if base + 0x2C > data.len() {
        return None;
    }

    // +0x00: cookie must be 0x55AA55AA
    let cookie = read_u32(data, base)?;
    if cookie != EFS_COOKIE {
        return None;
    }

    // +0x14: psp_dir_l1 pointer
    let psp_ptr = read_u32(data, base + 0x14)?;

    // +0x28: bios_dir_combo pointer (also checked for newer PSP combo placement)
    let bios_ptr = read_u32(data, base + 0x28)?;

    let psp_dir = resolve_dir(data, psp_ptr);
    let bios_dir = resolve_dir(data, bios_ptr);

    // If neither directory resolved, this EFS hit is likely garbage.
    if psp_dir.is_none() && bios_dir.is_none() {
        return None;
    }

    Some(AmdPspInfo {
        efs_offset,
        psp_dir,
        bios_dir,
    })
}

/// Attempt to read a directory header at the address stored in `ptr`.
///
/// AMD directory pointers are flash addresses; for a 16 MB image the bottom
/// 24 bits are the byte offset, but some images store a full 32-bit value
/// where the upper byte is 0xFF (SPI NOR unprogrammed). We mask to 24 bits
/// when the address is >= file size but fits in 24 bits.
fn resolve_dir(data: &[u8], ptr: u32) -> Option<AmdDir> {
    if ptr == 0 || ptr == 0xFFFF_FFFF || ptr == 0xFFFF_FFFE {
        return None;
    }

    // Try the raw pointer first; if it's out of range try the 24-bit mask.
    let offset = if (ptr as usize) + 8 <= data.len() {
        ptr as usize
    } else {
        let masked = (ptr & 0x00FF_FFFF) as usize;
        if masked + 8 <= data.len() {
            masked
        } else {
            return None;
        }
    };

    let dir_cookie = read_u32(data, offset)?;
    let cookie_str = cookie_name(dir_cookie)?.to_string();

    // Directory entry count is at +0x08 in both L1 and combo headers.
    // For a combo header it's the count of combo entries; for L1 it's direct.
    // Both formats store the count at the same position.
    let num_entries = read_u32(data, offset + 8).unwrap_or(0);

    Some(AmdDir {
        offset: offset as u32,
        cookie: cookie_str,
        num_entries,
    })
}

/// Print a human-readable summary of the AMD PSP/EFS structure.
pub fn print_amd_psp(info: &AmdPspInfo, file_size: u32) {
    println!("AMD Platform Security Processor (PSP)");
    println!("  EFS at     : {:#010x}", info.efs_offset);

    match &info.psp_dir {
        Some(d) => println!(
            "  PSP dir    : {:#010x}  [{}, {} entries]",
            d.offset, d.cookie, d.num_entries
        ),
        None => println!("  PSP dir    : not found"),
    }

    match &info.bios_dir {
        Some(d) => println!(
            "  BIOS dir   : {:#010x}  [{}, {} entries]",
            d.offset, d.cookie, d.num_entries
        ),
        None => println!("  BIOS dir   : not found"),
    }

    if file_size > 0 {
        let mb = file_size / (1024 * 1024);
        println!("  Flash size : {} MB", mb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_amd_psp_all_ff_returns_none() {
        // A buffer of 0xFF at each search offset — no valid EFS.
        let size = (*EFS_SEARCH_OFFSETS.last().unwrap() as usize) + 0x100;
        let data = vec![0xFF; size];
        assert!(scan_amd_psp(&data).is_none());
    }

    #[test]
    fn test_scan_amd_psp_detects_minimal_efs() {
        // Buffer large enough to cover the first search offset plus directory.
        let buf_size = 0x22000usize; // slightly past 0x20000 + headers
        let mut data = vec![0xFFu8; buf_size];

        let efs_base = 0x20000usize; // first canonical search offset

        // EFS cookie
        data[efs_base..efs_base + 4].copy_from_slice(&EFS_COOKIE.to_le_bytes());

        // PSP directory at 0x20100 — place a '$PSP' header there.
        let psp_dir_offset = 0x20100usize;
        let psp_dir_ptr = psp_dir_offset as u32;
        data[efs_base + 0x14..efs_base + 0x18].copy_from_slice(&psp_dir_ptr.to_le_bytes());
        data[psp_dir_offset..psp_dir_offset + 4]
            .copy_from_slice(&PSP_L1_COOKIE.to_le_bytes());
        // num_entries at +0x08
        data[psp_dir_offset + 8..psp_dir_offset + 12]
            .copy_from_slice(&3u32.to_le_bytes());

        // BIOS pointer — set to 0 (absent).
        data[efs_base + 0x28..efs_base + 0x2C].copy_from_slice(&0u32.to_le_bytes());

        let info = scan_amd_psp(&data).expect("should detect EFS");
        assert_eq!(info.efs_offset, 0x20000);

        let psp = info.psp_dir.as_ref().expect("PSP dir should be present");
        assert_eq!(psp.cookie, "$PSP");
        assert_eq!(psp.num_entries, 3);

        assert!(info.bios_dir.is_none());
    }
}
