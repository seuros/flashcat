#![allow(unexpected_cfgs)]

use anyhow::Result;
use state_machines::state_machine;
use tracing::info;

use crate::db::{self, ChipVoltage, SpiNorDef};
use crate::fpga::{self, Voltage};
use crate::spi::{self, detect};
use crate::usb::UsbDevice;

use super::SpiSpeed;

state_machine! {
    name: VoltageProbe,
    initial: Probing1v8,
    states: [Probing1v8, Probing3v3, Identified, NoChip],
    events {
        chip_found {
            transition: { from: Probing1v8, to: Identified }
            transition: { from: Probing3v3, to: Identified }
        }
        escalate {
            transition: { from: Probing1v8, to: Probing3v3 }
        }
        exhausted {
            transition: { from: Probing3v3, to: NoChip }
        }
    }
}

/// Probe 1.8V → 3.3V, return the device (already set up at the correct voltage)
/// plus the identified chip and the voltage it responded at.
///
/// Strategy per hardware safety analysis:
/// - 1.8V chip at 3.3V → overvoltage, can exceed abs-max (2.5V) and damage the chip
/// - 3.3V chip at 1.8V → undervoltage, may not respond but causes no damage
///
/// Therefore: probe 1.8V first, escalate to 3.3V only after full power-down.
pub async fn auto_probe(
    speed: SpiSpeed,
) -> Result<(UsbDevice, Option<&'static SpiNorDef>, Voltage)> {
    let dev = crate::usb::connect().await?;
    let probe = VoltageProbe::new(());

    // Phase 1: 1.8V (only on programmers that support it)
    if dev.kind.supports_voltage(Voltage::V1_8) {
        info!("auto-probe: trying 1.8V");
        fpga::load(&dev, Voltage::V1_8).await?;
        fpga::set_vcc(&dev, Voltage::V1_8).await?;
        spi::init(&dev, speed).await?;

        let id = detect::rdid(&dev).await?;
        let (mfr, id1, id2) = (id[0], id[1], id[2]);

        if mfr != 0xFF && mfr != 0x00 {
            match db::lookup(mfr, id1, id2)? {
                Some(chip) if chip.voltage == ChipVoltage::V1_8 => {
                    info!("auto-probe: identified {} at 1.8V", chip.name);
                    if chip.addr_bytes == 4 {
                        detect::enter_4byte_mode(&dev).await?;
                    }
                    let _m = probe.chip_found().unwrap();
                    return Ok((dev, Some(chip), Voltage::V1_8));
                }
                Some(chip) => {
                    // Known 3.3V chip responded — safe to drain and escalate
                    info!("auto-probe: known 3.3V chip {} responded at 1.8V, escalating", chip.name);
                }
                None => {
                    // Unknown chip responded at 1.8V — do NOT escalate to 3.3V.
                    // It may be a genuine 1.8V part missing from our DB; applying
                    // 3.3V could exceed its absolute maximum rating.
                    anyhow::bail!(
                        "unknown chip at 1.8V (RDID {mfr:#04x} {id1:#04x} {id2:#04x}) — \
                         refusing auto-escalation to 3.3V to protect the chip. \
                         Use --voltage 3v3 to override explicitly."
                    );
                }
            }
        }

        // No chip (or known 3.3V chip) at 1.8V.
        // Do NOT call vcc_off() here — LogicOff resets SSPI (fw 1.19) and the
        // subsequent fpga::load() SSPI_Init will fail.
        // fpga::load(V3_3) sends Logic3v3 which switches VCC; just wait for
        // the 1.8V rail cap to drain before applying 3.3V.
        info!("auto-probe: no 1.8V chip, waiting for VCC drain");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let probe = probe.escalate().unwrap();

    // Phase 2: 3.3V
    info!("auto-probe: trying 3.3V");
    fpga::load(&dev, Voltage::V3_3).await?;
    fpga::set_vcc(&dev, Voltage::V3_3).await?;
    spi::init(&dev, speed).await?;

    let id = detect::rdid(&dev).await?;
    let (mfr, id1, id2) = (id[0], id[1], id[2]);

    if mfr != 0xFF && mfr != 0x00 {
        if let Some(chip) = db::lookup(mfr, id1, id2)? {
            info!("auto-probe: identified {} at 3.3V", chip.name);
            if chip.addr_bytes == 4 {
                detect::enter_4byte_mode(&dev).await?;
            }
            let _m = probe.chip_found().unwrap();
            return Ok((dev, Some(chip), Voltage::V3_3));
        }
    }

    let _m = probe.exhausted().unwrap();
    info!("auto-probe: no chip found at either voltage");
    Ok((dev, None, Voltage::V3_3))
}
