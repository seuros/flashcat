# flashcat

Linux/FreeBSD/OpenBSD/NetBSD/macOS host tool for the **FlashcatUSB Pro** (PCB 5.x).
Replaces the original VB.NET Windows software.

## Requirements

- FlashcatUSB Pro connected via USB (`16c0:05e0`)
- Linux: udev rule to allow non-root access

```
SUBSYSTEM=="usb", ATTR{idVendor}=="16c0", ATTR{idProduct}=="05e0", MODE="0666"
```

## Install

```bash
cargo install --path .
```

## Usage

```bash
# Check connection and firmware version
flashcat check

# Identify attached chip (auto-detects voltage)
flashcat detect

# Read entire chip to file
flashcat read -f dump.bin

# Read 64KB at offset
flashcat read -f sector.bin --offset 0x10000 --length 0x10000

# Read a named region (uses embedded FMAP if no --layout given)
flashcat read -f bios.bin --region BIOS
flashcat read -f bios.bin --layout firmware.layout --region BIOS

# Erase chip, write, and verify
flashcat erase
flashcat write -f firmware.bin --verify

# Erase + write + verify in one pass
flashcat erase && flashcat write -f firmware.bin --erase --verify

# Compare flash against file (SHA-256 + diff report)
flashcat compare -f dump.bin

# Read and decode SFDP from chip
flashcat sfdp

# Show firmware region map from attached chip
flashcat fmap

# Show firmware region map from a local dump (no hardware needed)
flashcat fmap --file dump.bin

# List regions in a layout file (no hardware needed)
flashcat regions --file firmware.layout

# Read the chip's unique 64-bit serial number (counterfeit detection)
flashcat uid

# Protect entire chip (sets all BP bits, survives power cycle)
flashcat protect

# Remove all write protection (clears BP bits)
flashcat unprotect

# Lock all blocks globally — Winbond individual block lock (volatile, ~45ms)
flashcat block-lock --global

# Lock the sector/block at a specific address (volatile, resets on power cycle)
flashcat block-lock --addr 0x10000

# Unlock all blocks globally
flashcat block-unlock --global

# Unlock the sector/block at a specific address
flashcat block-unlock --addr 0x10000

# Watch for device plug-in and auto-identify chip
flashcat watch
```

### Global options

| Flag | Default | Description |
|------|---------|-------------|
| `--mhz` | `8` | SPI clock: 1, 2, 4, 8, 12, 16, 24, 32 (quad reads: 8, 16, 32 only) |
| `--voltage` | `auto` | Target voltage: `auto`, `1v8`, `3v3`, or `5v` |

### Read options

| Flag | Description |
|------|-------------|
| `--offset` | Start address (hex or decimal) |
| `--length` | Byte count to read |
| `--quad` | Quad SPI (4-bit) read — Mach1 hardware only |
| `--legacy-read` | Use Read (0x03) instead of Fast Read (0x0B) |
| `--layout <file>` | Load region definitions from a flashrom-format layout file |
| `--region <name>` | Read only the named region (auto-scans FMAP if no `--layout`) |

### Write options

`flashcat write` uses smart write by default — it reads the chip, skips matching sectors,
erases only what needs changing (using 64KB/32KB/4KB units as the chip supports), and writes
only the necessary pages. Use `--erase` to bypass smart and force a full erase before writing.

| Flag | Description |
|------|-------------|
| `--offset` | Start address |
| `--erase` | Force erase before writing, then raw write (bypasses smart comparison; use for pre-erased blank chips) |
| `--verify` | Read back and verify after writing |
| `--layout <file>` | Layout file for region selection |
| `--region <name>` | Write only the named region (file size must match region size exactly) |

### Erase options

| Flag | Description |
|------|-------------|
| `--offset` | Start address (omit for full chip erase) |
| `--length` | Byte count to erase (rounded up to erase unit boundary) |
| `--layout <file>` | Layout file for region selection |
| `--region <name>` | Erase only the named region |

## Voltage auto-detection

`--voltage auto` (default) probes at 1.8V first, then escalates to 3.3V after a safe
VCC drain if no 1.8V chip responds. Safety rules:

- **Unknown RDID at 1.8V with no SFDP** → hard stop. Use `--voltage 3v3` to override.
- **Ambiguous RDID at 1.8V** (multiple DB candidates, different voltages) → hard stop.
- **Known 3.3V chip responds at 1.8V** → safe escalation to 3.3V.

## Region support

Regions can be defined two ways:

**Layout file** (flashrom-compatible format):
```
0x00000000:0x000fffff bios
0x00100000:0x001fffff me
```

**Embedded FMAP** — coreboot and some vendor firmware embed a region map in the flash
image itself. `--region <name>` without `--layout` reads up to 4MB from the chip and
scans for the FMAP signature automatically.

## Firmware map analysis (`fmap`)

`flashcat fmap` reads from the attached chip. `flashcat fmap --file dump.bin` works
offline on a local binary — no hardware required.

Supported formats:

| Format | Description |
|--------|-------------|
| **FMAP** | coreboot / ChromeOS firmware map (`__FMAP__` signature) |
| **Intel IFD** | Intel Flash Descriptor (`FLVALSIG 0x0FF0A55A`) — covers FD, BIOS, ME, GbE, PDR regions |
| **AMD PSP** | AMD Embedded Firmware Structure (`0x55AA55AA`) — PSP and BIOS directory tables |
| **EFI FV** | Raw EFI Firmware Volumes (`_FVH`) — pre-IFD Apple Macs and bare BIOS region extracts |

## Write protection

`flashcat protect` sets all BP (block protect) bits in Status Register 1 to lock the entire
chip. `flashcat unprotect` clears them. Changes survive power cycles (non-volatile SR write).

Winbond chips ≤ 16MB use a 3-bit BP field; Winbond chips > 16MB (W25Q256 and up) use a
4-bit BP field — flashcat detects the geometry from the chip ID and applies the correct mask.

**Individual block lock** (Winbond W25Q series — volatile, resets on power cycle):

| Command | Effect |
|---------|--------|
| `block-lock --global` | Lock all blocks (~45ms, opcode 0x7E) |
| `block-lock --addr <addr>` | Lock the 64KB block containing `<addr>` (opcode 0x36) |
| `block-unlock --global` | Unlock all blocks (~45ms, opcode 0x98) |
| `block-unlock --addr <addr>` | Unlock the 64KB block containing `<addr>` (opcode 0x39) |

## SFDP fallback

If a chip's JEDEC ID is not in the built-in database, flashcat reads the chip's own
SFDP (Serial Flash Discoverable Parameters, JESD216) to determine geometry. This means
most standards-compliant SPI NOR flash will work even without a database entry.

Ambiguous RDIDs (multiple chips share the same 3-byte ID) are resolved using SFDP
density when possible.

## Unique ID and counterfeit detection

`flashcat uid` reads the chip's 64-bit factory serial number (opcode 0x4B). Winbond and most
modern SPI NOR flash support this. The output includes a counterfeit-likelihood assessment
based on whether the UID is blank (0x00…/0xFF…), a known bad pattern, or plausible.

## Supported chips

SPI NOR flash on Pro PCB5. Built-in database covers EON, Winbond, GigaDevice, Macronix,
Micron, Spansion, ISSI, and SST parts. Unknown chips fall back to SFDP auto-detection.
