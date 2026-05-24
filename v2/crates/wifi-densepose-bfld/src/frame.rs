//! `BfldFrame` wire-format primitives. See ADR-119.
//!
//! The header is `#[repr(C, packed)]` so the wire byte order is fixed across
//! x86_64, aarch64, and xtensa-esp32s3 — and so the witness-bundle pattern
//! (ADR-028) extends cleanly to BFLD frames.

use static_assertions::const_assert_eq;

/// Magic value identifying a `BfldFrame`. Reads as "BFLD" in hex-dump tools.
pub const BFLD_MAGIC: u32 = 0xBF1D_0001;

/// Current `BfldFrame` major version. Bumps on any incompatible layout change.
pub const BFLD_VERSION: u16 = 1;

/// Size of the packed header in bytes. Asserted at compile time below.
///
/// Note: ADR-119 AC1 initially claimed 40 bytes — that was a counting error.
/// Actual packed layout sums to 86. Updated 2026-05-24 to match implementation.
pub const BFLD_HEADER_SIZE: usize = 86;

/// Flag bits in `BfldFrameHeader::flags`. See ADR-119 §2.1.
pub mod flags {
    /// Payload contains an optional CSI delta section.
    pub const HAS_CSI_DELTA: u16 = 1 << 0;
    /// `privacy_mode` is engaged: identity-derived fields suppressed.
    pub const PRIVACY_MODE: u16 = 1 << 1;
    /// ESP32-S3 self-only adapter (ADR-123 §2.5): no `identity_risk_score`.
    pub const SELF_ONLY: u16 = 1 << 3;
}

/// On-the-wire BFLD frame header. 86 bytes, little-endian, packed.
///
/// All multi-byte integer fields are little-endian when serialized. The packed
/// layout guarantees zero internal padding; readers must use `read_unaligned`
/// (or the accessor helpers added in a later commit).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct BfldFrameHeader {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,
    pub timestamp_ns: u64,

    pub ap_hash: [u8; 16],
    pub sta_hash: [u8; 16],
    pub session_id: [u8; 16],

    pub channel: u16,
    pub bandwidth_mhz: u16,
    pub rssi_dbm: i16,
    pub noise_floor_dbm: i16,

    pub n_subcarriers: u16,
    pub n_tx: u8,
    pub n_rx: u8,
    pub quantization: u8,
    pub privacy_class: u8,

    pub payload_len: u32,
    pub payload_crc32: u32,
}

const_assert_eq!(core::mem::size_of::<BfldFrameHeader>(), BFLD_HEADER_SIZE);
