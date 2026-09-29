use crate::fpga::Voltage;

/// Hardware programmer variant detected from USB PID + firmware version byte.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Programmer {
    /// FlashcatUSB Classic (PCB 2.x) — ATmega32U2/U4, no FPGA
    /// Voltages: 3.3V, 5V
    /// PID: 0x05DE
    Classic,

    /// FlashcatUSB Pro (PCB 5.x) — ARM Cortex-M + Lattice iCE40 FPGA
    /// Voltages: 3.3V, 1.8V
    /// PID: 0x05E0
    Pro5,

    /// FlashcatUSB Mach1 (PCB 2.x) — ARM + Lattice iCE40 FPGA
    /// Voltages: 3.3V, 1.8V
    /// PID: 0x05E1
    Mach1,

    /// FlashcatUSB xPort — full-speed AVR, 12V chargepump for EPROM, no FPGA.
    /// Shares PID 0x05DE with the Classic; distinguished by firmware major
    /// version (xPort reports 5.x, Classic 4.x). IO protocol not yet
    /// reverse-engineered — voltage control STALLs.
    /// Voltages: 3.3V, 5V (+12V EPROM, unsupported)
    /// PID: 0x05DE
    Xport,
}

impl Programmer {
    /// Human-readable model name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Classic => "FlashcatUSB Classic",
            Self::Pro5    => "FlashcatUSB Pro",
            Self::Mach1   => "FlashcatUSB Mach1",
            Self::Xport   => "FlashcatUSB xPort",
        }
    }

    /// Short selector token used by `-p <model>`.
    pub fn token(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Pro5    => "pro",
            Self::Mach1   => "mach1",
            Self::Xport   => "xport",
        }
    }

    /// Whether this programmer has an FPGA that must be loaded each session.
    pub fn has_fpga(self) -> bool {
        matches!(self, Self::Pro5 | Self::Mach1)
    }

    /// Whether the firmware's SPI PAGE_PROGRAM lands one byte past the
    /// requested address (ARM/FPGA boards). The AVR boards program exactly.
    pub fn page_program_skews(self) -> bool {
        self.has_fpga()
    }

    /// Whether USB control transfers use Recipient::Interface (has_fpga)
    /// vs Recipient::Device (Classic).
    pub fn uses_interface_recipient(self) -> bool {
        self.has_fpga()
    }

    /// Supported target voltages for this programmer.
    pub fn supported_voltages(self) -> &'static [Voltage] {
        match self {
            Self::Classic => &[Voltage::V3_3, Voltage::V5_0],
            Self::Pro5    => &[Voltage::V3_3, Voltage::V1_8],
            Self::Mach1   => &[Voltage::V3_3, Voltage::V1_8],
            Self::Xport   => &[Voltage::V3_3, Voltage::V5_0],
        }
    }

    pub fn supports_voltage(self, v: Voltage) -> bool {
        self.supported_voltages().contains(&v)
    }
}
