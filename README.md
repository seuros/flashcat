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

# Identify attached chip
flashcat detect

# Read entire chip to file
flashcat read -f dump.bin

# Read 64KB at offset
flashcat read -f sector.bin --offset 0x10000 --length 0x10000

# Erase chip, write, and verify
flashcat erase
flashcat write -f firmware.bin --verify

# Compare flash against file (SHA-256 + diff report)
flashcat compare -f dump.bin
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--mhz` | `8` | SPI clock: 1, 2, 4, 8, 12, 16, 24, 32 |
| `--voltage` | `auto` | Target voltage: `auto`, `1v8`, `3v3`, or `5v` |

`auto` probes at 1.8V first, escalates to 3.3V after a safe VCC drain if no 1.8V
chip is found. Unknown RDID at 1.8V is a hard stop — use `--voltage 3v3` to override.

## Supported chips

SPI NOR flash on Pro PCB5. 37 chips supported (EON, Winbond, GigaDevice, Macronix,
Micron, Spansion, ISSI, SST). Unknown chips report raw RDID bytes.
