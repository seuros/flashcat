use anyhow::Result;

use crate::usb;

pub async fn cmd_devices() -> Result<()> {
    let list = usb::list_programmers().await?;
    if list.is_empty() {
        println!("No FlashcatUSB programmers attached.");
        return Ok(());
    }
    println!("{:<20} {:<8} {:<10} {:<14} {}", "MODEL", "-p", "PATH", "SERIAL", "FW");
    for (kind, path, serial, fw) in list {
        println!(
            "{:<20} {:<8} {:<10} {:<14} {}",
            kind.name(),
            kind.token(),
            path,
            serial.as_deref().unwrap_or("-"),
            fw
        );
    }
    Ok(())
}
