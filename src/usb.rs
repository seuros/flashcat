use anyhow::{bail, Context, Result};
use std::sync::OnceLock;
use std::time::Duration;

use crate::programmer::Programmer;

mod device;
mod requests;

pub use device::UsbDevice;
pub use requests::UsbReq;

pub const VID_EC: u16 = 0x16C0;
pub const PID_CLASSIC: u16 = 0x05DE; // shared by Classic and xPort
pub const PID_PRO: u16 = 0x05E0;
pub const PID_MACH1: u16 = 0x05E1;

/// Number of attempts to find and open the device. The previous session's
/// LogicOff (vcc_off) can briefly knock the firmware off the bus on some
/// hardware; we sleep and retry rather than failing the user immediately.
const CONNECT_ATTEMPTS: u32 = 6;
const CONNECT_BACKOFF_MS: u64 = 150;

/// VERSION budget when identifying a board; live firmware answers in a few ms.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a port-reset board may take to re-enumerate, polled in steps.
const RESET_SETTLE_MS: u64 = 3000;
const RESET_POLL_MS: u64 = 100;

/// How to pick a programmer when more than one is attached (`-p <value>`).
#[derive(Clone, Debug)]
pub enum DeviceSelector {
    /// Model token: classic, xport, mach1, pro.
    Model(Programmer),
    /// USB serial string (Mach1/Pro expose one; the 05de boards do not).
    Serial(String),
    /// USB topology path, e.g. `3-2.1` (busnum-port.chain).
    Path(String),
    /// Bare value: match against serial or path.
    Any(String),
}

impl DeviceSelector {
    /// Parse a `-p` value into a selector.
    pub fn parse(s: &str) -> Self {
        if let Some(v) = s.strip_prefix("serial=") {
            return Self::Serial(v.to_string());
        }
        if let Some(v) = s.strip_prefix("path=") {
            return Self::Path(v.to_string());
        }
        if let Some(m) = model_from_token(s) {
            return Self::Model(m);
        }
        Self::Any(s.to_string())
    }
}

fn model_from_token(s: &str) -> Option<Programmer> {
    match s.to_ascii_lowercase().as_str() {
        "classic" => Some(Programmer::Classic),
        "xport" => Some(Programmer::Xport),
        "mach1" => Some(Programmer::Mach1),
        "pro" | "pro5" => Some(Programmer::Pro5),
        _ => None,
    }
}

static SELECTOR: OnceLock<Option<DeviceSelector>> = OnceLock::new();

/// Set the process-wide programmer selector (from the global `-p` flag).
/// Call once in `main` before any `connect()`.
pub fn set_selector(sel: Option<DeviceSelector>) {
    let _ = SELECTOR.set(sel);
}

fn selector() -> Option<&'static DeviceSelector> {
    SELECTOR.get().and_then(|o| o.as_ref())
}

struct Cand {
    di: nusb::DeviceInfo,
    pid: u16,
}

/// `busnum-port.chain`, e.g. `3-2.1` — stable per physical port.
fn usb_path(di: &nusb::DeviceInfo) -> String {
    let ports = di
        .port_chain()
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(".");
    format!("{}-{}", di.busnum(), ports)
}

/// Resolve the model from PID alone; `None` for 0x05DE (needs a fw probe).
fn kind_from_pid(pid: u16) -> Option<Programmer> {
    match pid {
        PID_PRO => Some(Programmer::Pro5),
        PID_MACH1 => Some(Programmer::Mach1),
        _ => None,
    }
}

/// Resolve a 0x05DE board from its VERSION board-type byte (source: USB.vb
/// LoadFirmwareVersion). 'C'/'0' = Classic, 'X'/'E' = xPort. Unknown bytes
/// fall back to Classic — the two share an identical no-logic driver path, so
/// this only affects the displayed label.
fn kind_from_board_byte(b: u8) -> Programmer {
    match b {
        b'X' | b'E' => Programmer::Xport,
        _ => Programmer::Classic,
    }
}

async fn list_candidates() -> Result<Vec<Cand>> {
    Ok(nusb::list_devices()
        .await?
        .filter(|d| {
            d.vendor_id() == VID_EC
                && matches!(d.product_id(), p if p == PID_CLASSIC || p == PID_PRO || p == PID_MACH1)
        })
        .map(|di| {
            let pid = di.product_id();
            Cand { di, pid }
        })
        .collect())
}

/// Open, select config, claim interface 0, and switch to the bulk alt-setting
/// on FPGA models. `kind` decides the alt-setting; for a 0x05DE board pass
/// `Classic` and refine afterwards (Classic and xPort share the same setup).
async fn open_device(di: &nusb::DeviceInfo, kind: Programmer) -> Result<UsbDevice> {
    // Both USB speeds used the same 5ms inter-command delay, so keep it constant.
    let ctrl_delay = Duration::from_millis(5);
    let device = di.open().await?;
    // Mirrors official USB.vb:165 OpenUsbDevice — explicit configuration
    // selection before claiming. Some firmware revisions leave config 0
    // selected after LogicOff, which would refuse interface claims.
    // Errors here are best-effort; not all platforms support it.
    let _ = device.set_configuration(1).await;
    let iface = device.claim_interface(0).await?;
    // Pro/Mach1: bulk endpoints live in alternate setting 1.
    if kind.has_fpga() {
        iface.set_alt_setting(1).await?;
    }
    Ok(UsbDevice {
        device,
        iface,
        kind,
        ctrl_delay,
    })
}

/// Determine the model, opening the device to read the firmware version when
/// the PID alone can't tell (0x05DE → Classic 4.x vs xPort 5.x).
async fn probe_kind(di: &nusb::DeviceInfo) -> Result<Programmer> {
    if let Some(k) = kind_from_pid(di.product_id()) {
        return Ok(k);
    }
    let (_, board, _) = open_identified(di, Programmer::Classic)
        .await
        .context("cannot distinguish Classic vs xPort: firmware version read failed")?;
    Ok(kind_from_board_byte(board))
}

/// Open a board and read VERSION. A board whose firmware is wedged still
/// enumerates but ignores vendor requests; it gets one USB port reset and a
/// second try before giving up.
async fn open_identified(
    di: &nusb::DeviceInfo,
    kind: Programmer,
) -> Result<(UsbDevice, u8, String)> {
    let path = usb_path(di);
    let dev = open_device(di, kind).await?;
    let err = match dev.version_probe(PROBE_TIMEOUT).await {
        Ok((board, fw)) => return Ok((dev, board, fw)),
        Err(e) => e,
    };
    tracing::warn!("{path}: no response to VERSION ({err:#}) — resetting USB port");
    let device = dev.device.clone();
    drop(dev);
    // A reset that makes the board re-enumerate reports the old handle as
    // disconnected; that is the reset working, not failing.
    match device.reset().await {
        Ok(()) => {}
        Err(e) if e.kind() == nusb::ErrorKind::Disconnected => {}
        Err(e) => return Err(e).with_context(|| format!("{path}: USB port reset failed")),
    }
    drop(device);

    let deadline = tokio::time::Instant::now() + Duration::from_millis(RESET_SETTLE_MS);
    let cand = loop {
        tokio::time::sleep(Duration::from_millis(RESET_POLL_MS)).await;
        if let Some(c) = list_candidates()
            .await?
            .into_iter()
            .find(|c| usb_path(&c.di) == path)
        {
            break c;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("{path}: board did not come back after USB reset — replug it");
        }
    };
    let dev = open_device(&cand.di, kind).await?;
    let (board, fw) = dev
        .version_probe(PROBE_TIMEOUT)
        .await
        .with_context(|| format!("{path}: firmware unresponsive even after USB reset — replug it"))?;
    Ok((dev, board, fw))
}

async fn cand_matches(c: &Cand, sel: &DeviceSelector) -> bool {
    match sel {
        DeviceSelector::Model(m) => match kind_from_pid(c.pid) {
            Some(k) => k == *m,
            // Only probe a 05de board when the target model lives there.
            None if matches!(m, Programmer::Classic | Programmer::Xport) => {
                probe_kind(&c.di).await.map(|k| k == *m).unwrap_or(false)
            }
            None => false,
        },
        DeviceSelector::Serial(s) => c.di.serial_number() == Some(s.as_str()),
        DeviceSelector::Path(p) => usb_path(&c.di) == *p,
        DeviceSelector::Any(v) => {
            c.di.serial_number() == Some(v.as_str()) || usb_path(&c.di) == *v
        }
    }
}

/// Human-readable listing for the disambiguation error (probes each board).
async fn describe_all(cands: &[Cand]) -> String {
    let mut out = String::new();
    for c in cands {
        let kind = probe_kind(&c.di).await.ok();
        let token = kind.map(|k| k.token()).unwrap_or("?");
        let model = kind
            .map(|k| k.name())
            .unwrap_or("FlashcatUSB (probe failed)");
        let serial = c.di.serial_number().unwrap_or("-");
        out.push_str(&format!(
            "  -p {:<8} {} (PID {:#06x}, path {}, serial {})\n",
            token,
            model,
            c.pid,
            usb_path(&c.di),
            serial
        ));
    }
    out
}

pub async fn connect() -> Result<UsbDevice> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..CONNECT_ATTEMPTS {
        let cands = list_candidates().await?;
        if cands.is_empty() {
            // Could be mid-reenumeration from the previous session's LogicOff.
            last_err = Some(anyhow::anyhow!(
                "FlashcatUSB not found — check USB connection and udev rules"
            ));
            tokio::time::sleep(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            continue;
        }

        // Pick the target. Selection errors (ambiguous / no match) are
        // deterministic given the bus state, so they bail immediately rather
        // than spinning the retry loop.
        let cand: &Cand = match selector() {
            Some(sel) => {
                let mut matched: Vec<&Cand> = Vec::new();
                for c in &cands {
                    if cand_matches(c, sel).await {
                        matched.push(c);
                    }
                }
                match matched.len() {
                    1 => matched[0],
                    0 => bail!(
                        "no attached programmer matches -p selector\nattached:\n{}",
                        describe_all(&cands).await
                    ),
                    n => bail!(
                        "-p selector matches {n} programmers — be more specific:\n{}",
                        describe_all(&cands).await
                    ),
                }
            }
            None if cands.len() == 1 => {
                // Guard against a sibling programmer mid-reenumeration: confirm
                // the singleton is stable before auto-using it, so we never
                // silently pick one of several attached boards.
                tokio::time::sleep(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
                let recheck = list_candidates().await?;
                if recheck.len() > 1 {
                    bail!(
                        "{} programmers attached — pick one with -p <model|serial|path>:\n{}",
                        recheck.len(),
                        describe_all(&recheck).await
                    );
                }
                &cands[0]
            }
            None => bail!(
                "{} programmers attached — pick one with -p <model|serial|path>:\n{}",
                cands.len(),
                describe_all(&cands).await
            ),
        };

        // Open and finalize the model (refine 0x05DE → Classic/xPort). A failed
        // version read here must error, never silently fall back to Classic.
        let attempt_result: Result<UsbDevice> = async {
            if cand.pid != PID_CLASSIC {
                let kind = kind_from_pid(cand.pid).unwrap_or(Programmer::Classic);
                return open_device(&cand.di, kind).await;
            }
            let (mut dev, board, _) = open_identified(&cand.di, Programmer::Classic)
                .await
                .context("cannot distinguish Classic vs xPort: firmware version read failed")?;
            dev.kind = kind_from_board_byte(board);
            Ok(dev)
        }
        .await;

        match attempt_result {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                tracing::debug!("connect attempt {}/{}: {e}", attempt + 1, CONNECT_ATTEMPTS);
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("FlashcatUSB connect failed after retries")))
}

/// One attached board as the `devices` command reports it.
pub struct ProgrammerInfo {
    pub path: String,
    pub serial: Option<String>,
    /// `(kind, fw)`, or why the board could not be identified.
    pub ident: Result<(Programmer, String)>,
}

/// Enumerate attached programmers for the `devices` command. A board that
/// fails to identify is reported, not fatal to the listing.
pub async fn list_programmers() -> Result<Vec<ProgrammerInfo>> {
    let cands = list_candidates().await?;
    let mut out = Vec::new();
    for c in &cands {
        let provisional = kind_from_pid(c.pid).unwrap_or(Programmer::Classic);
        let ident = open_identified(&c.di, provisional).await.map(|(_, board, fw)| {
            let kind = kind_from_pid(c.pid).unwrap_or_else(|| kind_from_board_byte(board));
            (kind, fw)
        });
        out.push(ProgrammerInfo {
            path: usb_path(&c.di),
            serial: c.di.serial_number().map(|s| s.to_string()),
            ident,
        });
    }
    Ok(out)
}
