#![warn(clippy::all)]

/// EFI Firmware Volume (FV) parser.
///
/// Scans for `_FVH` signature (0x5F465648 / bytes 5F 46 56 48) and parses
/// `EFI_FIRMWARE_VOLUME_HEADER` from the UEFI PI specification.
///
/// Used by pre-IFD Apple Macs and other EFI-only images that carry raw FVs
/// without an Intel Flash Descriptor wrapper.

const FVH_SIGNATURE: u32 = 0x4856_465F; // '_FVH' little-endian
const FVH_SIG_OFFSET: u32 = 0x28; // offset of Signature field within FV header
const MAX_FV_COUNT: usize = 32;
const MAX_FV_LENGTH: u64 = 256 * 1024 * 1024; // 256 MB sanity cap
const MIN_FV_LENGTH: u64 = 0x48; // minimum valid FV header size

#[derive(Debug, Clone)]
pub struct EfiFv {
    pub offset: u32,     // FV start offset in file
    pub length: u64,     // FvLength from header
    pub guid: [u8; 16],  // FileSystemGuid
    pub revision: u8,
    #[allow(dead_code)]
    pub attributes: u32,
}

#[derive(Debug)]
pub struct EfiFvInfo {
    pub volumes: Vec<EfiFv>,
}

/// Scan `data` for valid EFI Firmware Volumes.
///
/// Returns `None` if no valid, page-aligned FV is found.
pub fn scan_efifv(data: &[u8]) -> Option<EfiFvInfo> {
    let mut volumes = Vec::new();

    // Walk every byte looking for the 4-byte `_FVH` signature.
    let sig_bytes = FVH_SIGNATURE.to_le_bytes();
    let search_end = data.len().saturating_sub(3);

    let mut i = 0;
    while i < search_end {
        if data[i..i + 4] != sig_bytes {
            i += 1;
            continue;
        }

        // The signature lives at FV+0x28, so the FV header starts 0x28 bytes earlier.
        if (i as u32) < FVH_SIG_OFFSET {
            i += 1;
            continue;
        }
        let fv_start = i as u32 - FVH_SIG_OFFSET;

        // FVs must be page-aligned (4096 bytes).
        if fv_start != 0 && fv_start % 4096 != 0 {
            i += 1;
            continue;
        }

        if let Some(fv) = parse_fv(data, fv_start) {
            volumes.push(fv);
            if volumes.len() >= MAX_FV_COUNT {
                break;
            }
        }

        i += 1;
    }

    if volumes.is_empty() {
        None
    } else {
        Some(EfiFvInfo { volumes })
    }
}

fn parse_fv(data: &[u8], fv_start: u32) -> Option<EfiFv> {
    let start = fv_start as usize;

    // Need at least MIN_FV_LENGTH bytes from fv_start.
    if start + MIN_FV_LENGTH as usize > data.len() {
        return None;
    }

    // +0x10: FileSystemGuid (16 bytes)
    let mut guid = [0u8; 16];
    guid.copy_from_slice(&data[start + 0x10..start + 0x20]);

    // +0x20: FvLength (u64 LE)
    let fv_length = u64::from_le_bytes(
        data[start + 0x20..start + 0x28].try_into().ok()?,
    );

    // Sanity-check FvLength.
    if fv_length < MIN_FV_LENGTH || fv_length > MAX_FV_LENGTH {
        return None;
    }

    // The entire FV must fit within the file.
    let fv_end = (fv_start as u64).checked_add(fv_length)?;
    if fv_end > data.len() as u64 {
        return None;
    }

    // +0x2C: Attributes (u32 LE)
    let attributes = u32::from_le_bytes(
        data[start + 0x2C..start + 0x30].try_into().ok()?,
    );

    // +0x37: Revision (u8)
    let revision = data[start + 0x37];

    Some(EfiFv {
        offset: fv_start,
        length: fv_length,
        guid,
        revision,
        attributes,
    })
}

/// Format a 16-byte EFI GUID as `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
///
/// The wire format is: first 4 bytes as u32 LE, next 2 as u16 LE, next 2 as
/// u16 LE, then 8 bytes big-endian.
fn format_guid(g: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        d1, d2, d3, g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15],
    )
}

/// Print a human-readable table of the discovered EFI Firmware Volumes.
pub fn print_efifv(info: &EfiFvInfo, _file_size: u32) {
    println!("EFI Firmware Volumes");
    println!("  volumes: {}", info.volumes.len());
    println!();
    println!(
        "  {:<3}  {:<10}  {:<10}  {:<36}  {}",
        "#", "OFFSET", "LENGTH", "GUID", "REV"
    );
    println!(
        "  {:<3}  {:<10}  {:<10}  {:<36}  {}",
        "--", "----------", "----------", "------------------------------------", "---"
    );
    for (i, fv) in info.volumes.iter().enumerate() {
        println!(
            "  {:<3}  {:#010x}  {:#010x}  {:<36}  {}",
            i,
            fv.offset,
            fv.length,
            format_guid(&fv.guid),
            fv.revision,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_efifv_all_ff_returns_none() {
        assert!(scan_efifv(&[0xFF; 4096]).is_none());
    }

    #[test]
    fn test_scan_efifv_detects_minimal_fv() {
        // Build a 4096-byte buffer with a minimal valid FV header at offset 0.
        let mut data = vec![0u8; 4096];

        // +0x10: FileSystemGuid — arbitrary non-zero GUID
        data[0x10..0x20].copy_from_slice(&[
            0xD9, 0x54, 0x93, 0x7A, 0x68, 0x04, 0x4A, 0x44,
            0x81, 0xCE, 0x0B, 0xF6, 0x17, 0xD8, 0x90, 0xDF,
        ]);

        // +0x20: FvLength = 0x1000 (4096 bytes, fits exactly)
        let fv_length: u64 = 0x1000;
        data[0x20..0x28].copy_from_slice(&fv_length.to_le_bytes());

        // +0x28: Signature = '_FVH'
        data[0x28..0x2C].copy_from_slice(&FVH_SIGNATURE.to_le_bytes());

        // +0x2C: Attributes
        data[0x2C..0x30].copy_from_slice(&0x0004_F6FFu32.to_le_bytes());

        // +0x37: Revision = 2
        data[0x37] = 2;

        let info = scan_efifv(&data).expect("should detect FV");
        assert_eq!(info.volumes.len(), 1);
        let fv = &info.volumes[0];
        assert_eq!(fv.offset, 0);
        assert_eq!(fv.length, fv_length);
        assert_eq!(fv.revision, 2);
    }
}
