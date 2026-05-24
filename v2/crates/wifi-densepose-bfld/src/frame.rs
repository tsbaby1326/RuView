//! `BfldFrame` wire-format primitives. See ADR-119.
//!
//! The header is `#[repr(C, packed)]` so the wire byte order is fixed across
//! x86_64, aarch64, and xtensa-esp32s3 — and so the witness-bundle pattern
//! (ADR-028) extends cleanly to BFLD frames.
//!
//! All multi-byte integers serialize as **little-endian**. The
//! `to_le_bytes`/`from_le_bytes` helpers encode/decode without `unsafe`, which
//! is forbidden in this crate; the encoded bytes are the canonical wire form.

use static_assertions::const_assert_eq;

use crate::BfldError;

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
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BfldFrameHeader {
    /// Must equal [`BFLD_MAGIC`].
    pub magic: u32,
    /// Layout version. Currently [`BFLD_VERSION`].
    pub version: u16,
    /// Flag bits — see [`flags`].
    pub flags: u16,
    /// Monotonic capture-clock timestamp in nanoseconds.
    pub timestamp_ns: u64,
    /// BLAKE3-keyed(site_salt, ap_mac)[0..16] — ADR-120 §2.3.
    pub ap_hash: [u8; 16],
    /// BLAKE3-keyed(site_salt ‖ day_epoch, sta_mac)[0..16] — daily-rotated.
    pub sta_hash: [u8; 16],
    /// Ephemeral session identifier, rotated on capture-session boundary.
    pub session_id: [u8; 16],
    /// 802.11 channel number.
    pub channel: u16,
    /// Channel bandwidth in MHz: 20 / 40 / 80 / 160.
    pub bandwidth_mhz: u16,
    /// Received signal strength in dBm.
    pub rssi_dbm: i16,
    /// Noise floor in dBm.
    pub noise_floor_dbm: i16,
    /// Number of OFDM subcarriers represented.
    pub n_subcarriers: u16,
    /// Number of transmit antennas.
    pub n_tx: u8,
    /// Number of receive antennas.
    pub n_rx: u8,
    /// 0=f32, 1=i16, 2=i8, 3=packed (4-bit nibbles).
    pub quantization: u8,
    /// `PrivacyClass` byte — see ADR-120 §2.1.
    pub privacy_class: u8,
    /// Length of the payload section in bytes.
    pub payload_len: u32,
    /// CRC-32/ISO-HDLC over payload bytes only.
    pub payload_crc32: u32,
}

const_assert_eq!(core::mem::size_of::<BfldFrameHeader>(), BFLD_HEADER_SIZE);

impl BfldFrameHeader {
    /// Build a header with `magic` and `version` already set correctly.
    /// All other fields default to zero — caller fills them in.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            magic: BFLD_MAGIC,
            version: BFLD_VERSION,
            ..Self::default()
        }
    }

    /// Serialize to canonical little-endian wire form (86 bytes).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn to_le_bytes(&self) -> [u8; BFLD_HEADER_SIZE] {
        let mut buf = [0u8; BFLD_HEADER_SIZE];
        let mut o = 0usize;

        // Copy locally to dodge `#[repr(packed)]` unaligned-borrow warnings.
        let magic = self.magic;
        let version = self.version;
        let flags = self.flags;
        let timestamp_ns = self.timestamp_ns;
        let channel = self.channel;
        let bandwidth_mhz = self.bandwidth_mhz;
        let rssi_dbm = self.rssi_dbm;
        let noise_floor_dbm = self.noise_floor_dbm;
        let n_subcarriers = self.n_subcarriers;
        let payload_len = self.payload_len;
        let payload_crc32 = self.payload_crc32;

        buf[o..o + 4].copy_from_slice(&magic.to_le_bytes()); o += 4;
        buf[o..o + 2].copy_from_slice(&version.to_le_bytes()); o += 2;
        buf[o..o + 2].copy_from_slice(&flags.to_le_bytes()); o += 2;
        buf[o..o + 8].copy_from_slice(&timestamp_ns.to_le_bytes()); o += 8;
        buf[o..o + 16].copy_from_slice(&self.ap_hash); o += 16;
        buf[o..o + 16].copy_from_slice(&self.sta_hash); o += 16;
        buf[o..o + 16].copy_from_slice(&self.session_id); o += 16;
        buf[o..o + 2].copy_from_slice(&channel.to_le_bytes()); o += 2;
        buf[o..o + 2].copy_from_slice(&bandwidth_mhz.to_le_bytes()); o += 2;
        buf[o..o + 2].copy_from_slice(&rssi_dbm.to_le_bytes()); o += 2;
        buf[o..o + 2].copy_from_slice(&noise_floor_dbm.to_le_bytes()); o += 2;
        buf[o..o + 2].copy_from_slice(&n_subcarriers.to_le_bytes()); o += 2;
        buf[o] = self.n_tx; o += 1;
        buf[o] = self.n_rx; o += 1;
        buf[o] = self.quantization; o += 1;
        buf[o] = self.privacy_class; o += 1;
        buf[o..o + 4].copy_from_slice(&payload_len.to_le_bytes()); o += 4;
        buf[o..o + 4].copy_from_slice(&payload_crc32.to_le_bytes()); o += 4;

        debug_assert_eq!(o, BFLD_HEADER_SIZE);
        buf
    }

    /// Parse from canonical little-endian wire form.
    ///
    /// Returns [`BfldError::InvalidMagic`] if the magic prefix is wrong, and
    /// [`BfldError::UnsupportedVersion`] for a version this build cannot decode.
    /// Field-level validation (CRC, payload_len bounds) is deliberately *not*
    /// performed here — that lives at the frame-level parser.
    pub fn from_le_bytes(bytes: &[u8; BFLD_HEADER_SIZE]) -> Result<Self, BfldError> {
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != BFLD_MAGIC {
            return Err(BfldError::InvalidMagic(magic));
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if version != BFLD_VERSION {
            return Err(BfldError::UnsupportedVersion(version));
        }

        let mut h = Self {
            magic,
            version,
            flags: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            timestamp_ns: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            ap_hash: [0; 16],
            sta_hash: [0; 16],
            session_id: [0; 16],
            channel: u16::from_le_bytes(bytes[64..66].try_into().unwrap()),
            bandwidth_mhz: u16::from_le_bytes(bytes[66..68].try_into().unwrap()),
            rssi_dbm: i16::from_le_bytes(bytes[68..70].try_into().unwrap()),
            noise_floor_dbm: i16::from_le_bytes(bytes[70..72].try_into().unwrap()),
            n_subcarriers: u16::from_le_bytes(bytes[72..74].try_into().unwrap()),
            n_tx: bytes[74],
            n_rx: bytes[75],
            quantization: bytes[76],
            privacy_class: bytes[77],
            payload_len: u32::from_le_bytes(bytes[78..82].try_into().unwrap()),
            payload_crc32: u32::from_le_bytes(bytes[82..86].try_into().unwrap()),
        };
        h.ap_hash.copy_from_slice(&bytes[16..32]);
        h.sta_hash.copy_from_slice(&bytes[32..48]);
        h.session_id.copy_from_slice(&bytes[48..64]);
        Ok(h)
    }
}
