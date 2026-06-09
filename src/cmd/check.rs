use anyhow::Result;

use crate::usb;

pub async fn cmd_check() -> Result<()> {
    let dev = usb::connect().await?;
    println!("Connected: {}", dev.kind.name());
    let ver = dev.firmware_version().await?;
    println!("Firmware:  {ver}");
    dev.echo().await?;
    println!("Echo:      OK");
    Ok(())
}
