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

# Watch for device plug-in and auto-identify chip
flashcat watch
```

### Global options

| Flag | Default | Description |
|------|---------|-------------|
| `--mhz` | `8` | SPI clock: 1, 2, 4, 8, 12, 16, 24, 32 |
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

| Flag | Description |
|------|-------------|
| `--offset` | Start address |
| `--erase` | Erase affected sectors before writing; full-chip images erase automatically |
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

## SFDP fallback

If a chip's JEDEC ID is not in the built-in database, flashcat reads the chip's own
SFDP (Serial Flash Discoverable Parameters, JESD216) to determine geometry. This means
most standards-compliant SPI NOR flash will work even without a database entry.

Ambiguous RDIDs (multiple chips share the same 3-byte ID) are resolved using SFDP
density when possible.

## Supported chips

SPI NOR flash on Pro PCB5. Built-in database covers EON, Winbond, GigaDevice, Macronix,
Micron, Spansion, ISSI, and SST parts. Unknown chips fall back to SFDP auto-detection.
