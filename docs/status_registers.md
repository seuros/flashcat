# SPI NOR Status Register Layouts — Vendor Reference

Extracted from authoritative manufacturer datasheets (cached in `docs/datasheets/`).
Used to drive `flashcat status` per-family decoders. **Bit indices below are
register-local (bit 0 = LSB of that register).**

## Universal baseline (all SPI NOR)

| Opcode | Register | Notes |
|--------|----------|-------|
| `0x05` | SR1 | RDSR — always exists |
| `0x01` | WRSR | Always exists |

| SR1 bit | Name | Meaning | Universal? |
|---------|------|---------|------------|
| 0 | WIP / BUSY | program/erase/WRSR in progress | yes |
| 1 | WEL | write enable latch | yes |
| 2..7 | vendor-defined | — | NO |

## Winbond W25Q128JV (Rev H) — `docs/datasheets/W25Q128JV-RevH.pdf`

**Opcodes**: RDSR1=`0x05`, RDSR2=`0x35`, RDSR3=`0x15`; WRSR1=`0x01` (also writes SR2 atomically if 2-byte payload), WRSR2=`0x31`, WRSR3=`0x11`. Volatile WREN=`0x50`.

### SR1 (`0x05`)
| Bit | Name | Type |
|-----|------|------|
| 0 | BUSY | RO |
| 1 | WEL | RO |
| 2 | BP0 | NV/Vol writable |
| 3 | BP1 | NV/Vol writable |
| 4 | BP2 | NV/Vol writable |
| 5 | TB (top/bottom) | NV/Vol writable |
| 6 | SEC (4KB sector vs 64KB block protect) | NV/Vol writable |
| 7 | SRP0 | NV/Vol writable |

### SR2 (`0x35`) — bits S8..S15
| Bit (local) | Name | Type |
|-------------|------|------|
| 0 | SRL (SRP1) | NV/Vol writable |
| 1 | **QE** | NV/Vol OTP writable |
| 2 | reserved | — |
| 3 | LB1 | NV OTP writable |
| 4 | LB2 | NV OTP writable |
| 5 | LB3 | NV OTP writable |
| 6 | CMP (complement protect) | NV/Vol writable |
| 7 | SUS (suspend status) | RO |

### SR3 (`0x15`) — bits S16..S23
| Bit (local) | Name | Type |
|-------------|------|------|
| 0 | reserved | — |
| 1 | reserved | — |
| 2 | WPS (BP-mode vs individual lock) | NV/Vol writable |
| 3 | reserved | — |
| 4 | reserved | — |
| 5 | DRV0 | NV/Vol writable |
| 6 | DRV1 | NV/Vol writable |
| 7 | HOLD/RST | NV/Vol writable |

DRV[1:0]: 00=100%, 01=75%, 10=50%, 11=25%.

## GigaDevice GD25Q128E (Rev 1.2) — `docs/datasheets/GD25Q128E-Rev1.2.pdf`

**Opcodes**: RDSR1=`0x05`, RDSR2=`0x35`, RDSR3=`0x15`; WRSR1=`0x01`, WRSR2=`0x31`, WRSR3=`0x11`. Volatile WREN=`0x50`.

Layout matches Winbond closely **with two differences**:
- BP field is **5 bits**: BP0..BP4 = SR1 bits 2..6. SR1 bit 7 = SRP0. (Winbond W25Q128 uses 3-bit BP + TB + SEC; W25Q256JV uses 4-bit BP.)
- SR2 bit 2 is **SUS2** (program suspend), not reserved. SR2 bit 7 is **SUS1** (erase suspend). Winbond has a single SUS at SR2[7].
- SR3 bit 0 = **DC** (dummy cycle config). SR3 bits 1..4 reserved. DRV0=S21, DRV1=S22, HOLD/RST=S23. So local SR3[0]=DC vs Winbond SR3[2]=WPS.

### GD25Q128E SR3 (`0x15`)
| Bit | Name |
|-----|------|
| 0 | DC (dummy config) |
| 1..4 | reserved |
| 5 | DRV0 |
| 6 | DRV1 |
| 7 | HOLD/RST |

**Watch out**: GD25Q128E SR3 does **not** expose WPS — individual-block-lock scheme on GD is selected differently (or absent) compared to Winbond. Don't decode SR3[2]=WPS on GigaDevice.

## Macronix MX25L12835F (v1.7) — `docs/datasheets/MX25L12835F-v1.7.pdf`

**Opcodes**: RDSR=`0x05`, RDCR=`0x15`, WRSR=`0x01` (accepts 1- or 2-byte payload: SR then CR). **No SR2 at `0x35`.**

### SR (`0x05`) — single status register
| Bit | Name | Type |
|-----|------|------|
| 0 | WIP | RO volatile |
| 1 | WEL | RO volatile |
| 2 | BP0 | NV |
| 3 | BP1 | NV |
| 4 | BP2 | NV |
| 5 | BP3 | NV |
| 6 | **QE** | NV |
| 7 | SRWD | NV |

**Critical divergence**: Macronix puts QE at **SR bit 6**, not SR2[1]. Code must
branch on vendor before decoding. Reading `0x35` on Macronix is undefined — do not.

### CR (`0x15`) — configuration register, separate from any "SR3"
| Bit | Name | Type |
|-----|------|------|
| 0 | ODS0 | volatile |
| 1 | ODS1 | volatile |
| 2 | ODS2 | volatile |
| 3 | TB (top/bottom protect) | OTP |
| 4 | reserved | — |
| 5 | reserved | — |
| 6 | DC0 (dummy cycle) | volatile |
| 7 | DC1 (dummy cycle) | volatile |

ODS[2:0]: 001=90Ω, 010=60Ω, 011=45Ω, 101=20Ω, 110=15Ω, 111=30Ω (default), 000/100 reserved.

**Same opcode `0x15`, totally different layout vs Winbond/GD.** Decoder must
key off vendor before interpreting `0x15`.

## SFDP (`0x5B`) — universal discovery — `docs/datasheets/JESD216.pdf`, `docs/datasheets/Macronix-AN114-SFDP.pdf`

`0x5B` (some vendors `0x5A`), 3-byte address (24-bit), then 1 dummy byte, then read.

SFDP header at offset 0:
- bytes 0..3: signature `"SFDP"` (`0x50 0x44 0x46 0x53` little-endian as read)
- byte 4: SFDP minor rev
- byte 5: SFDP major rev
- byte 6: NPH = number of parameter headers minus 1
- byte 7: access protocol / reserved

Each parameter header (8 bytes):
- byte 0: ID LSB (JEDEC BFPT = `0xFF`)
- byte 1: param table minor rev
- byte 2: param table major rev
- byte 3: param table length in DWORDs
- bytes 4..6: param table pointer (24-bit, byte address)
- byte 7: ID MSB (BFPT = `0xFF`; total BFPT id = `0xFF00`)

JEDEC Basic Flash Parameter Table (BFPT) DWORDs (rev B, 9 DWORDs; rev D adds up to 20):
- DWORD 1: erase size support, write granularity, volatile SR support, 4KB erase opcode, address bytes, fast read 1-1-2 / 1-2-2 / 1-1-4 / 1-4-4 / 2-2-2 / 4-4-4 support flags
- DWORD 2: flash density in **bits − 1** (32-bit unsigned)
- DWORD 3..7: per-fast-read mode dummy cycles + opcodes
- DWORD 8..9: erase type 1..4 size (2^N) + opcode
- (rev C/D extends: DWORD 11 = page size, chip erase timing; DWORD 14 = power-down; DWORD 15 = QE method, 4-byte addressing)

## Implementation rules for `flashcat status`

1. Resolve vendor from `ResolvedChip.vendor` first.
2. Read SR1 (`0x05`) — always safe.
3. Branch:
   - Winbond / GigaDevice: also read `0x35` and `0x15`, decode per Winbond table (with GD's SR3[0]=DC override).
   - Macronix: also read `0x15` as **CR** (not SR3), decode separately.
   - Unknown vendor: print SR1 with universal bits (WIP/WEL) + raw hex of SR1. Do not blindly read `0x35`/`0x15` — opcodes may have different meanings on other vendors (Micron uses `0x85`/`0xB5` for NV/Volatile config).
4. SFDP fallback can confirm presence of QE bit method (JESD216 DWORD 15, bits 22:20) when vendor is unknown.

## Sources

- [W25Q128JV Rev H](https://www.winbond.com/resource-files/W25Q128JV%20RevH%2003102021%20Plus.pdf)
- [MX25L12835F v1.7](https://www.macronix.com/Lists/Datasheet/Attachments/9173/MX25L12835F,%203V,%20128Mb,%20v1.7.pdf)
- [GD25Q128E Rev 1.2](https://www.gigadevice.com.cn/Public/Uploads/uploadfile/files/20220714/DS-00480-GD25Q128E-Rev1.2.pdf)
- [JESD216 (v1.0) mirror](https://www.taterli.com/wp-content/uploads/2017/07/JESD216.pdf)
- [Macronix AN-114 SFDP Introduction](https://www.macronix.com/Lists/ApplicationNote/Attachments/1870/AN114v1-SFDP%20Introduction.pdf)
