use anyhow::Result;
use std::path::PathBuf;

use crate::bios::layout;

pub async fn cmd_regions(layout_file: PathBuf) -> Result<()> {
    let regions = layout::parse_layout_file(&layout_file)?;

    if regions.is_empty() {
        println!("No regions found in {}", layout_file.display());
        return Ok(());
    }

    println!("{:<24} {:<12} {:<12}  SIZE", "NAME", "START", "END");
    println!("{}", "-".repeat(60));

    for r in &regions {
        println!(
            "  {:<24} {:#010x}..{:#010x}  ({} bytes)",
            r.name,
            r.offset,
            r.end_inclusive(),
            r.length,
        );
    }

    Ok(())
}
