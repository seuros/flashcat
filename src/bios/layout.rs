#![warn(clippy::all)]

use anyhow::{bail, Result};
use std::path::Path;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Region
// ---------------------------------------------------------------------------

/// Named address range within a flash chip.
pub struct Region {
    pub name: String,
    pub offset: u32,
    pub length: u32,
}

impl Region {
    pub fn end_inclusive(&self) -> u32 {
        self.offset + self.length - 1
    }
}

// ---------------------------------------------------------------------------
// FMAP constants
// ---------------------------------------------------------------------------

pub const FMAP_SIGNATURE: &[u8; 8] = b"__FMAP__";
pub const FMAP_HEADER_SIZE: usize = 56;
pub const FMAP_AREA_SIZE: usize = 42;

// ---------------------------------------------------------------------------
// FMAP structs
// ---------------------------------------------------------------------------

pub struct FmapHeader {
    pub ver_major: u8,
    pub ver_minor: u8,
    pub base: u64,
    pub size: u32,
    pub name: String,
    pub nareas: u16,
}

pub struct FmapArea {
    pub offset: u32,
    pub size: u32,
    pub name: String,
    pub flags: u16,
}

// ---------------------------------------------------------------------------
// Layout file parser  (flashrom format: 0xSTART:0xEND name)
// ---------------------------------------------------------------------------

pub fn parse_hex_or_dec_u32(s: &str) -> Result<u32> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).map_err(|e| anyhow::anyhow!("{e}"))
    } else {
        s.parse::<u32>().map_err(|e| anyhow::anyhow!("{e}"))
    }
}

pub fn parse_layout_file(path: &Path) -> Result<Vec<Region>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read layout file {}: {e}", path.display()))?;

    let mut regions = Vec::new();

    for (lineno, line) in raw.lines().enumerate() {
        let lineno = lineno + 1; // 1-based for error messages
        // Normalise CRLF — already handled by .lines(), but strip inline comments
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Strip inline comment
        let line = match line.split_once('#') {
            Some((pre, _)) => pre.trim(),
            None => line,
        };

        // Expected: RANGE NAME  where RANGE = START:END
        let mut parts = line.splitn(2, char::is_whitespace);
        let range = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("line {lineno}: empty range"))?;
        let name = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("line {lineno}: missing region name"))?
            .trim()
            .to_string();

        if name.is_empty() {
            bail!("line {lineno}: missing region name");
        }

        let (start_s, end_s) = range
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("line {lineno}: expected START:END, got '{range}'"))?;

        let start = parse_hex_or_dec_u32(start_s)
            .map_err(|e| anyhow::anyhow!("line {lineno}: invalid start address '{start_s}': {e}"))?;
        let end = parse_hex_or_dec_u32(end_s)
            .map_err(|e| anyhow::anyhow!("line {lineno}: invalid end address '{end_s}': {e}"))?;

        if end < start {
            bail!("line {lineno}: end {end:#x} < start {start:#x}");
        }

        regions.push(Region {
            name,
            offset: start,
            length: end - start + 1, // end is inclusive
        });
    }

    Ok(regions)
}

// ---------------------------------------------------------------------------
// FMAP scanner
// ---------------------------------------------------------------------------

fn parse_name_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn scan_fmap(data: &[u8]) -> Option<(FmapHeader, Vec<FmapArea>)> {
    // Find signature
    let sig_pos = data
        .windows(8)
        .position(|w| w == FMAP_SIGNATURE.as_slice())?;

    let hdr_end = sig_pos + FMAP_HEADER_SIZE;
    if data.len() < hdr_end {
        return None;
    }

    let hdr = &data[sig_pos..hdr_end];
    // Layout (offsets relative to sig_pos):
    //  0..8   signature (already checked)
    //  8      ver_major
    //  9      ver_minor
    // 10..18  base u64 LE
    // 18..22  size u32 LE
    // 22..54  name (32 bytes, null-padded)
    // 54..56  nareas u16 LE

    let ver_major = hdr[8];
    let ver_minor = hdr[9];
    let base = u64::from_le_bytes(hdr[10..18].try_into().ok()?);
    let size = u32::from_le_bytes(hdr[18..22].try_into().ok()?);
    let name = parse_name_field(&hdr[22..54]);
    let nareas = u16::from_le_bytes(hdr[54..56].try_into().ok()?);

    // Sanity cap
    if nareas > 256 {
        return None;
    }

    let areas_start = hdr_end;
    let areas_end = areas_start + FMAP_AREA_SIZE * nareas as usize;
    if data.len() < areas_end {
        return None;
    }

    let mut areas = Vec::with_capacity(nareas as usize);
    for i in 0..nareas as usize {
        let a = &data[areas_start + i * FMAP_AREA_SIZE..areas_start + (i + 1) * FMAP_AREA_SIZE];
        // Area layout (42 bytes):
        //  0..4   offset u32 LE
        //  4..8   size   u32 LE
        //  8..40  name   (32 bytes, null-padded)
        // 40..42  flags  u16 LE
        let area_offset = u32::from_le_bytes(a[0..4].try_into().ok()?);
        let area_size = u32::from_le_bytes(a[4..8].try_into().ok()?);
        let area_name = parse_name_field(&a[8..40]);
        let flags = u16::from_le_bytes(a[40..42].try_into().ok()?);

        // FMAP area.offset is relative to header.base; compute SPI offset
        let _spi_offset = area_offset.saturating_sub(base as u32);

        areas.push(FmapArea {
            offset: area_offset,
            size: area_size,
            name: area_name,
            flags,
        });
    }

    let header = FmapHeader {
        ver_major,
        ver_minor,
        base,
        size,
        name,
        nareas,
    };

    Some((header, areas))
}

// ---------------------------------------------------------------------------
// FMAP → Region conversion
// ---------------------------------------------------------------------------

pub fn fmap_to_regions(header: &FmapHeader, areas: &[FmapArea]) -> Vec<Region> {
    areas
        .iter()
        .map(|a| {
            let spi_offset = a.offset.saturating_sub(header.base as u32);
            Region {
                name: a.name.clone(),
                offset: spi_offset,
                length: a.size,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Region lookup
// ---------------------------------------------------------------------------

pub fn find_region<'a>(regions: &'a [Region], name: &str) -> Option<&'a Region> {
    regions
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------
// Shared region resolution
// ---------------------------------------------------------------------------

pub enum RegionSource {
    LayoutFile(PathBuf),
    FmapScan,
}

pub async fn resolve_region(
    source: RegionSource,
    name: &str,
    chip: &crate::ResolvedChip,
    dev: &crate::usb::UsbDevice,
    speed: crate::spi::SpiSpeed,
) -> Result<Region> {
    let _ = speed; // used implicitly via spi::read below

    let regions: Vec<Region> = match source {
        RegionSource::LayoutFile(path) => parse_layout_file(&path)?,
        RegionSource::FmapScan => {
            let scan_limit = chip.size_bytes.min(4 * 1024 * 1024);
            let data = crate::spi::read(dev, chip, 0, scan_limit, false).await?;
            match scan_fmap(&data) {
                Some((hdr, areas)) => fmap_to_regions(&hdr, &areas),
                None => bail!("no FMAP signature found in flash"),
            }
        }
    };

    match find_region(&regions, name) {
        Some(r) => {
            // Validate bounds
            let end = r.offset.checked_add(r.length).ok_or_else(|| {
                anyhow::anyhow!("region '{}' address range overflows u32", r.name)
            })?;
            if end > chip.size_bytes {
                bail!(
                    "region '{}' ({:#010x}..{:#010x}) exceeds chip size {:#x}",
                    r.name,
                    r.offset,
                    end,
                    chip.size_bytes
                );
            }
            Ok(Region {
                name: r.name.clone(),
                offset: r.offset,
                length: r.length,
            })
        }
        None => {
            let available: Vec<&str> = regions.iter().map(|r| r.name.as_str()).collect();
            bail!(
                "region '{}' not found — available: {}",
                name,
                available.join(", ")
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Helper: write a temp layout file and parse it
    fn with_layout(content: &str) -> Result<Vec<Region>> {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        parse_layout_file(f.path())
    }

    #[test]
    fn test_layout_basic() {
        let regions = with_layout("0x000000:0x0fffff BIOS\n0x100000:0x1fffff EC\n").unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].name, "BIOS");
        assert_eq!(regions[0].offset, 0x000000);
        assert_eq!(regions[0].length, 0x100000);
        assert_eq!(regions[1].name, "EC");
        assert_eq!(regions[1].offset, 0x100000);
        assert_eq!(regions[1].length, 0x100000);
    }

    #[test]
    fn test_layout_comment_and_blank() {
        let regions = with_layout(
            "# full map\n\n0x000000:0x0fffff BIOS # main bios region\n",
        )
        .unwrap();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].name, "BIOS");
    }

    #[test]
    fn test_layout_crlf() {
        let content = "0x000000:0x0fffff BIOS\r\n0x100000:0x1fffff EC\r\n";
        let regions = with_layout(content).unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].name, "BIOS");
        assert_eq!(regions[1].name, "EC");
    }

    #[test]
    fn test_layout_decimal() {
        // Decimal addresses without 0x prefix
        let regions = with_layout("0:65535 BOOT\n").unwrap();
        assert_eq!(regions[0].offset, 0);
        assert_eq!(regions[0].length, 65536);
    }

    #[test]
    fn test_scan_fmap_basic() {
        // Build a minimal FMAP blob: header (56 bytes) + 1 area (42 bytes)
        let mut blob = Vec::new();

        // Signature
        blob.extend_from_slice(b"__FMAP__");
        // ver_major, ver_minor
        blob.push(1);
        blob.push(0);
        // base (u64 LE) = 0
        blob.extend_from_slice(&0u64.to_le_bytes());
        // size (u32 LE) = 8 MB
        blob.extend_from_slice(&(8u32 * 1024 * 1024).to_le_bytes());
        // name (32 bytes, null-padded)
        let mut name_buf = [0u8; 32];
        name_buf[..9].copy_from_slice(b"TEST_FMAP");
        blob.extend_from_slice(&name_buf);
        // nareas (u16 LE) = 1
        blob.extend_from_slice(&1u16.to_le_bytes());

        assert_eq!(blob.len(), FMAP_HEADER_SIZE);

        // Area 0: offset=0, size=0x10000, name=REGION0, flags=0
        blob.extend_from_slice(&0u32.to_le_bytes()); // offset
        blob.extend_from_slice(&0x10000u32.to_le_bytes()); // size
        let mut aname = [0u8; 32];
        aname[..7].copy_from_slice(b"REGION0");
        blob.extend_from_slice(&aname);
        blob.extend_from_slice(&0u16.to_le_bytes()); // flags

        assert_eq!(blob.len(), FMAP_HEADER_SIZE + FMAP_AREA_SIZE);

        let result = scan_fmap(&blob);
        assert!(result.is_some());
        let (hdr, areas) = result.unwrap();

        assert_eq!(hdr.ver_major, 1);
        assert_eq!(hdr.ver_minor, 0);
        assert_eq!(hdr.base, 0);
        assert_eq!(hdr.size, 8 * 1024 * 1024);
        assert_eq!(hdr.name, "TEST_FMAP");
        assert_eq!(hdr.nareas, 1);

        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].name, "REGION0");
        assert_eq!(areas[0].offset, 0);
        assert_eq!(areas[0].size, 0x10000);
        assert_eq!(areas[0].flags, 0);
    }

    #[test]
    fn test_scan_fmap_with_prefix() {
        // Blob with junk before the FMAP signature
        let mut blob = vec![0xFFu8; 128];
        blob.extend_from_slice(b"__FMAP__");
        blob.push(1);
        blob.push(0);
        blob.extend_from_slice(&0u64.to_le_bytes());
        blob.extend_from_slice(&(4u32 * 1024 * 1024).to_le_bytes());
        let mut name_buf = [0u8; 32];
        name_buf[..8].copy_from_slice(b"CHROMEOS");
        blob.extend_from_slice(&name_buf);
        blob.extend_from_slice(&0u16.to_le_bytes()); // nareas = 0

        let result = scan_fmap(&blob);
        assert!(result.is_some());
        let (hdr, areas) = result.unwrap();
        assert_eq!(hdr.name, "CHROMEOS");
        assert_eq!(areas.len(), 0);
    }

    #[test]
    fn test_scan_fmap_too_short() {
        // Only signature, no header bytes
        let blob = b"__FMAP__".to_vec();
        assert!(scan_fmap(&blob).is_none());
    }

    #[test]
    fn test_scan_fmap_nareas_sanity_cap() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"__FMAP__");
        blob.push(1);
        blob.push(0);
        blob.extend_from_slice(&0u64.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&[0u8; 32]); // name
        blob.extend_from_slice(&512u16.to_le_bytes()); // nareas = 512 > 256 → None
        assert!(scan_fmap(&blob).is_none());
    }

    #[test]
    fn test_find_region_case_insensitive() {
        let regions = vec![
            Region { name: "BIOS".to_string(), offset: 0, length: 0x10000 },
        ];
        assert!(find_region(&regions, "bios").is_some());
        assert!(find_region(&regions, "BIOS").is_some());
        assert!(find_region(&regions, "missing").is_none());
    }
}
