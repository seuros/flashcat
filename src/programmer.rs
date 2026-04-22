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
}

impl Programmer {
    /// Whether this programmer has an FPGA that must be loaded each session.
    pub fn has_fpga(self) -> bool {
        matches!(self, Self::Pro5 | Self::Mach1)
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
        }
    }

    pub fn supports_voltage(self, v: Voltage) -> bool {
        self.supported_voltages().contains(&v)
    }
}
