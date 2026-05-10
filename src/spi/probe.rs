#![allow(unexpected_cfgs)]

use anyhow::Result;
use state_machines::state_machine;
use tracing::{info, warn};

use crate::chip::{ParamSource, ResolvedChip};
use crate::db::{self, ChipVoltage};
use crate::fpga::{self, Voltage};
use crate::power_down_and_vcc_off;
use crate::spi::{self, detect, sfdp};
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

/// Voltage comparison helper.
fn voltage_matches_chip(voltage: Voltage, chip_voltage: ChipVoltage) -> bool {
    matches!(
        (voltage, chip_voltage),
        (Voltage::V1_8, ChipVoltage::V1_8)
            | (Voltage::V3_3, ChipVoltage::V3_3)
            | (Voltage::V5_0, ChipVoltage::V3_3)
    )
}

/// Internal result of chip resolution at a given voltage.
enum Resolved {
    /// Chip identified and voltage matches.
    Match(ResolvedChip),
    /// Chip identified but voltage does not match the current probe level.
    WrongVoltage(ResolvedChip),
    /// No chip (RDID blank, or no DB/SFDP info available).
    None,
}

/// Resolve a chip at the given voltage: DB lookup first, SFDP fallback.
///
/// DB lookup may return multiple matches (ambiguous RDID). When all candidates
/// agree on voltage the first one is used. When they disagree SFDP size is
/// used to narrow the candidates. If still ambiguous: hard-stop at 1.8V,
/// warn + SFDP-only at 3.3V.
///
/// At 1.8V: unknown RDID + valid SFDP = genuine 1.8V chip not in DB → accept.
/// At 3.3V: unknown RDID + valid SFDP = chip described by itself → accept.
/// At 1.8V: unknown RDID + no SFDP = could be a 1.8V part → hard stop, don't escalate.
async fn resolve_chip(
    dev: &UsbDevice,
    rdid: [u8; 3],
    voltage: Voltage,
) -> Result<Resolved> {
    let (mfr, id1, id2) = (rdid[0], rdid[1], rdid[2]);

    // 1. DB lookup — may return 0, 1, or >1 matches.
    let matches = db::lookup(mfr, id1, id2)?;

    match matches.len() {
        0 => {
            // Not in DB — try SFDP.
            info!("auto-probe: RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} not in DB, trying SFDP");
            match sfdp::try_read_sfdp(dev).await {
                Some(info) => {
                    let chip = sfdp::sfdp_to_resolved(&info, rdid, voltage);
                    Ok(Resolved::Match(chip))
                }
                None => Ok(Resolved::None),
            }
        }

        1 => {
            // Unambiguous DB match.
            let db_chip = matches[0];
            if voltage_matches_chip(voltage, db_chip.voltage) {
                // Try SFDP to improve geometry; fall back to DB-only if unavailable.
                let chip = match sfdp::try_read_sfdp(dev).await {
                    Some(sfdp_info) => sfdp::merge_db_with_sfdp(db_chip, &sfdp_info),
                    None => {
                        // Many genuine pre-JESD216 parts have no SFDP at all (early
                        // Macronix MX25L6406E, older Winbond W25Q*). The official
                        // fcusb app never even queries SFDP. Don't paint these as
                        // counterfeit — only log at info level.
                        info!(
                            "{} identified via RDID; SFDP not available — using DB parameters",
                            db_chip.name
                        );
                        db_chip_to_resolved(db_chip)
                    }
                };
                Ok(Resolved::Match(chip))
            } else {
                // Known chip, wrong voltage for this probe level.
                Ok(Resolved::WrongVoltage(db_chip_to_resolved(db_chip)))
            }
        }

        _ => {
            // Ambiguous RDID: multiple DB entries share the same RDID triple.
            info!(
                "auto-probe: RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} is ambiguous ({} DB candidates)",
                matches.len()
            );

            // Check if all candidates agree on voltage.
            let voltages_agree = matches.windows(2).all(|w| w[0].voltage == w[1].voltage);

            if voltages_agree {
                // All candidates share the same voltage; filter to voltage-matching ones first.
                let voltage_matches: Vec<_> = matches
                    .iter()
                    .filter(|c| voltage_matches_chip(voltage, c.voltage))
                    .collect();

                if voltage_matches.is_empty() {
                    // No candidate matches this voltage level.
                    return Ok(Resolved::WrongVoltage(db_chip_to_resolved(matches[0])));
                }

                // Check whether all voltage-matching candidates agree on size.
                let all_same_size = voltage_matches
                    .windows(2)
                    .all(|w| w[0].size_bytes == w[1].size_bytes);

                if all_same_size {
                    // Sizes agree — use SFDP to enrich geometry, or fall back to DB.
                    let db_chip = voltage_matches[0];
                    let chip = match sfdp::try_read_sfdp(dev).await {
                        Some(sfdp_info) => sfdp::merge_db_with_sfdp(db_chip, &sfdp_info),
                        None => {
                            warn!(
                                "{} identified via RDID but SFDP is absent — \
                                 using DB parameters only (possible counterfeit or non-JESD216 chip)",
                                db_chip.name
                            );
                            db_chip_to_resolved(db_chip)
                        }
                    };
                    Ok(Resolved::Match(chip))
                } else {
                    // Same voltage, different sizes — must use SFDP to pick the right one.
                    warn!(
                        "RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} matches {} DB entries with \
                         different sizes — trying SFDP to disambiguate",
                        voltage_matches.len()
                    );
                    match sfdp::try_read_sfdp(dev).await {
                        Some(sfdp_info) => {
                            let sfdp_size = sfdp_info.size_bytes;
                            if let Some(candidate) =
                                voltage_matches.iter().find(|c| c.size_bytes == sfdp_size)
                            {
                                // SFDP size uniquely identifies one candidate.
                                info!(
                                    "auto-probe: size ambiguity resolved via SFDP ({} bytes) → {}",
                                    sfdp_size, candidate.name
                                );
                                let mut chip = sfdp::merge_db_with_sfdp(candidate, &sfdp_info);
                                chip.source = ParamSource::DatabaseWithSfdp;
                                Ok(Resolved::Match(chip))
                            } else {
                                // SFDP size doesn't match any DB candidate — trust SFDP directly.
                                warn!(
                                    "SFDP size {} bytes matches no DB candidate for \
                                     RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} — using SFDP-only params",
                                    sfdp_size
                                );
                                let chip = sfdp::sfdp_to_resolved(&sfdp_info, rdid, voltage);
                                Ok(Resolved::Match(chip))
                            }
                        }
                        None => {
                            // No SFDP available — pick the smallest candidate (conservative).
                            let smallest = voltage_matches
                                .iter()
                                .min_by_key(|c| c.size_bytes)
                                .unwrap();
                            warn!(
                                "SFDP unavailable for ambiguous RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} \
                                 — using smallest match: {} ({} bytes). \
                                 Use `sfdp` command for disambiguation.",
                                smallest.name, smallest.size_bytes
                            );
                            Ok(Resolved::Match(db_chip_to_resolved(smallest)))
                        }
                    }
                }
            } else {
                // Candidates disagree on voltage — try SFDP size to disambiguate.
                match sfdp::try_read_sfdp(dev).await {
                    Some(sfdp_info) => {
                        let sfdp_size = sfdp_info.size_bytes;
                        let size_matches: Vec<_> = matches
                            .iter()
                            .filter(|c| c.size_bytes == sfdp_size)
                            .collect();

                        match size_matches.len() {
                            1 => {
                                // SFDP size uniquely resolves the ambiguity.
                                let db_chip = size_matches[0];
                                info!(
                                    "auto-probe: RDID ambiguity resolved via SFDP size ({} bytes) → {}",
                                    sfdp_size, db_chip.name
                                );
                                let chip = sfdp::merge_db_with_sfdp(db_chip, &sfdp_info);
                                if voltage_matches_chip(voltage, db_chip.voltage) {
                                    Ok(Resolved::Match(chip))
                                } else {
                                    Ok(Resolved::WrongVoltage(chip))
                                }
                            }
                            _ => {
                                // Still ambiguous even after SFDP — treat as non-authoritative.
                                warn!(
                                    "RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} ambiguous after SFDP: \
                                     {} candidates still match",
                                    size_matches.len().max(matches.len())
                                );
                                match voltage {
                                    Voltage::V1_8 => {
                                        // Hard stop: can't risk overvoltage on unknown 1.8V chip.
                                        anyhow::bail!(
                                            "ambiguous RDID {mfr:#04x} {id1:#04x} {id2:#04x} at \
                                             1.8V — multiple candidates, refusing escalation to 3.3V \
                                             to protect the chip. Use --voltage explicitly."
                                        );
                                    }
                                    _ => {
                                        // At 3.3V: warn and use SFDP-only params.
                                        warn!(
                                            "using SFDP-only parameters for ambiguous RDID \
                                             {mfr:#04x}:{id1:#04x}:{id2:#04x} at 3.3V"
                                        );
                                        let chip =
                                            sfdp::sfdp_to_resolved(&sfdp_info, rdid, voltage);
                                        Ok(Resolved::Match(chip))
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // Ambiguous and no SFDP.
                        match voltage {
                            Voltage::V1_8 => {
                                anyhow::bail!(
                                    "ambiguous RDID {mfr:#04x} {id1:#04x} {id2:#04x} at 1.8V — \
                                     no SFDP available, refusing escalation to 3.3V to protect the \
                                     chip. Use --voltage explicitly."
                                );
                            }
                            _ => {
                                // Pick the first 3.3V candidate if any.
                                let candidate = matches
                                    .iter()
                                    .find(|c| c.voltage == ChipVoltage::V3_3)
                                    .or_else(|| matches.first())
                                    .copied();
                                match candidate {
                                    Some(db_chip) => {
                                        warn!(
                                            "ambiguous RDID {mfr:#04x}:{id1:#04x}:{id2:#04x} — \
                                             no SFDP, picking {} as best 3.3V guess",
                                            db_chip.name
                                        );
                                        let chip = db_chip_to_resolved(db_chip);
                                        if voltage_matches_chip(voltage, db_chip.voltage) {
                                            Ok(Resolved::Match(chip))
                                        } else {
                                            Ok(Resolved::WrongVoltage(chip))
                                        }
                                    }
                                    None => Ok(Resolved::None),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Convert a `&SpiNorDef` DB entry to a `ResolvedChip` using only DB data.
pub(crate) fn db_chip_to_resolved_pub(db: &'static crate::db::SpiNorDef) -> ResolvedChip {
    db_chip_to_resolved(db)
}

fn db_chip_to_resolved(db: &'static crate::db::SpiNorDef) -> ResolvedChip {
    // Manufacturers confirmed by datasheet to support 0x52 (32KB Block Erase) and
    // 0xD8 (64KB Block Erase) for all their 3-byte 4KB-granularity parts in our DB.
    // Any manufacturer not listed here gets only the 4KB entry as a safe fallback;
    // unknown future entries must not silently inherit 32K/64K erase support.
    const FAMILIES_WITH_STANDARD_BLOCK_ERASE: &[u8] = &[
        0xEF, // Winbond W25Q (JV/BV/FW series)
        0xC8, // GigaDevice GD25Q/GD25LQ
        0xC2, // Macronix MX25L
        0x20, // Micron N25Q / ST
        0x9D, // ISSI IS25LP
        0x1C, // EON EN25QH/EN25Q
        0x01, // Spansion / Cypress / Infineon S25FL
        0xBF, // SST / Microchip SST25VF
    ];

    let erase_types = if db.erase_size <= 4096
        && db.addr_bytes == 3
        && FAMILIES_WITH_STANDARD_BLOCK_ERASE.contains(&db.mfr)
    {
        vec![
            crate::chip::EraseType { size_bytes: 4096,  opcode: 0x20 },
            crate::chip::EraseType { size_bytes: 32768, opcode: 0x52 },
            crate::chip::EraseType { size_bytes: 65536, opcode: 0xD8 },
        ]
    } else {
        // 4-byte chips, 64KB-unit chips, or unknown manufacturer: single safe entry.
        vec![crate::chip::EraseType {
            size_bytes: db.erase_size,
            opcode: if db.addr_bytes == 4 {
                if db.erase_size <= 4096 { 0x21 } else { 0xDC }
            } else {
                if db.erase_size <= 4096 { 0x20 } else { 0xD8 }
            },
        }]
    };

    ResolvedChip {
        name: db.name.clone(),
        mfr: db.mfr,
        id1: db.id1,
        id2: db.id2,
        voltage: db.voltage,
        size_bytes: db.size_bytes,
        page_size: db.page_size,
        erase_size: db.erase_size,
        erase_types,
        addr_bytes: db.addr_bytes,
        quad: db.quad,
        source: ParamSource::Database,
        chip_erase_max_ms: None, // no SFDP — fallback formula used at erase time
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
/// SFDP is used as fallback when RDID is not in the DB.
pub async fn auto_probe(
    speed: SpiSpeed,
) -> Result<(UsbDevice, Option<ResolvedChip>, Voltage)> {
    let dev = crate::usb::connect().await?;
    let probe = VoltageProbe::new(());

    enum Phase1Probe {
        Found(ResolvedChip),
        Escalate,
    }

    // Phase 1: 1.8V (only on programmers that support it)
    if dev.kind.supports_voltage(Voltage::V1_8) {
        info!("auto-probe: trying 1.8V");
        let phase1: Result<Phase1Probe> = async {
            fpga::load(&dev, Voltage::V1_8).await?;
            fpga::set_vcc(&dev, Voltage::V1_8).await?;
            spi::init(&dev, speed).await?;

            let mut id = detect::rdid(&dev).await?;
            if id[0] == 0xFF || id[0] == 0x00 {
                tracing::debug!("RDID blank ({:#04x}) at {:?}, attempting software reset", id[0], Voltage::V1_8);
                detect::software_reset(&dev).await?;
                id = detect::rdid(&dev).await?;
            }
            let (mfr, id1, id2) = (id[0], id[1], id[2]);
            info!("auto-probe: 1.8V RDID = {mfr:#04x} {id1:#04x} {id2:#04x}");

            if mfr != 0xFF && mfr != 0x00 {
                match resolve_chip(&dev, id, Voltage::V1_8).await? {
                    Resolved::Match(chip) => {
                        info!("auto-probe: identified {} at 1.8V", chip.name);
                        if chip.addr_bytes == 4 {
                            detect::enter_4byte_mode(&dev).await?;
                        }
                        return Ok(Phase1Probe::Found(chip));
                    }
                    Resolved::WrongVoltage(chip) => {
                        // Known 3.3V chip responded at 1.8V — safe to escalate
                        info!(
                            "auto-probe: known 3.3V chip {} responded at 1.8V, escalating",
                            chip.name
                        );
                    }
                    Resolved::None => {
                        // Unknown chip, SFDP also failed — hard stop.
                        // Can't determine if this is a 1.8V part; escalating to 3.3V could destroy it.
                        anyhow::bail!(
                            "unknown chip at 1.8V (RDID {mfr:#04x} {id1:#04x} {id2:#04x}) — \
                             no SFDP response, refusing escalation to 3.3V to protect the chip. \
                             Use --voltage 3v3 to override explicitly."
                        );
                    }
                }
            }

            Ok(Phase1Probe::Escalate)
        }.await;

        match phase1 {
            Ok(Phase1Probe::Found(chip)) => {
                let _m = probe.chip_found().map_err(|_| anyhow::anyhow!("probe state machine error: unexpected chip_found transition"))?;
                return Ok((dev, Some(chip), Voltage::V1_8));
            }
            Ok(Phase1Probe::Escalate) => {}
            Err(e) => {
                power_down_and_vcc_off(&dev).await;
                return Err(e);
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

    let probe = probe.escalate().map_err(|_| anyhow::anyhow!("probe state machine error: unexpected escalate transition"))?;

    // Phase 2: 3.3V — wrap in async block so any error gets cleanup before propagating.
    info!("auto-probe: trying 3.3V");
    let phase2: anyhow::Result<Option<ResolvedChip>> = async {
        fpga::load(&dev, Voltage::V3_3).await?;
        fpga::set_vcc(&dev, Voltage::V3_3).await?;
        spi::init(&dev, speed).await?;

        let mut id = detect::rdid(&dev).await?;
        if id[0] == 0xFF || id[0] == 0x00 {
            tracing::debug!("RDID blank ({:#04x}) at {:?}, attempting software reset", id[0], Voltage::V3_3);
            detect::software_reset(&dev).await?;
            id = detect::rdid(&dev).await?;
        }
        let (mfr, id1, id2) = (id[0], id[1], id[2]);
        info!("auto-probe: 3.3V RDID = {mfr:#04x} {id1:#04x} {id2:#04x}");
        let _ = (id1, id2); // used in log above; id array used below

        if mfr == 0xFF || mfr == 0x00 {
            return Ok(None);
        }

        match resolve_chip(&dev, id, Voltage::V3_3).await? {
            Resolved::Match(chip) => {
                info!("auto-probe: identified {} at 3.3V", chip.name);
                if chip.addr_bytes == 4 {
                    detect::enter_4byte_mode(&dev).await?;
                }
                Ok(Some(chip))
            }
            Resolved::WrongVoltage(chip) => {
                // 1.8V chip found at 3.3V probe — shouldn't happen since we checked 1.8V first
                // but protect it anyway
                anyhow::bail!(
                    "chip {} requires 1.8V but is being probed at 3.3V — use --voltage 1v8",
                    chip.name
                )
            }
            Resolved::None => Ok(None),
        }
    }.await;

    match phase2 {
        Ok(Some(chip)) => {
            let _m = probe.chip_found().map_err(|_| anyhow::anyhow!("probe state machine error: unexpected chip_found transition"))?;
            return Ok((dev, Some(chip), Voltage::V3_3));
        }
        Ok(None) => {}
        Err(e) => {
            power_down_and_vcc_off(&dev).await;
            return Err(e);
        }
    }

    let _m = probe.exhausted().map_err(|_| anyhow::anyhow!("probe state machine error: unexpected exhausted transition"))?;
    info!("auto-probe: no chip found at either voltage");
    power_down_and_vcc_off(&dev).await;
    Ok((dev, None, Voltage::V3_3))
}
