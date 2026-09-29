use anyhow::Result;

use crate::usb;

pub async fn cmd_devices() -> Result<()> {
    let list = usb::list_programmers().await?;
    if list.is_empty() {
        println!("No FlashcatUSB programmers attached.");
        return Ok(());
    }
    println!("{:<20} {:<8} {:<10} {:<14} {}", "MODEL", "-p", "PATH", "SERIAL", "FW");
    for p in list {
        let serial = p.serial.as_deref().unwrap_or("-");
        match p.ident {
            Ok((kind, fw)) => println!(
                "{:<20} {:<8} {:<10} {:<14} {}",
                kind.name(),
                kind.token(),
                p.path,
                serial,
                fw
            ),
            Err(e) => println!(
                "{:<20} {:<8} {:<10} {:<14} {}",
                "(unresponsive)",
                "?",
                p.path,
                serial,
                format!("{e:#}")
            ),
        }
    }
    Ok(())
}
