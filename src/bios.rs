/// Firmware image format parsers — format-agnostic, work on raw bytes
/// regardless of how the data was acquired (SPI, NAND, file, network).
pub mod amd_psp;
pub mod efifv;
pub mod ifd;
pub mod layout;
