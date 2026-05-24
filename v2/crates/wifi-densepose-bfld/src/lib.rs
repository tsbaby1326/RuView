//! # BFLD — Beamforming Feedback Layer for Detection
//!
//! Privacy-gated WiFi sensing primitives derived from 802.11ac/ax Beamforming
//! Feedback Information (BFI). See [`docs/adr/ADR-118-bfld-beamforming-feedback-layer-for-detection.md`](../../../docs/adr/ADR-118-bfld-beamforming-feedback-layer-for-detection.md).
//!
//! ## Three structural invariants
//!
//! - **I1**: Raw BFI never exits the node.
//! - **I2**: Identity embedding is in-RAM-only.
//! - **I3**: Cross-site identity correlation is cryptographically impossible.
//!
//! Status: P1 scaffold — frame format only. P2–P6 land in subsequent commits.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod frame;

pub use frame::{BfldFrameHeader, BFLD_MAGIC, BFLD_VERSION, BFLD_HEADER_SIZE};

/// Privacy classification carried in every `BfldFrame`. See ADR-120 §2.1.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivacyClass {
    /// Local-only research data including raw BFI matrix. Never networked.
    Raw = 0,
    /// Operator-acknowledged research mode over LAN. Downsampled angles +
    /// identity_embedding + identity_risk_score available. Required for
    /// Soul Signature deployments (ADR-120 §2.7).
    Derived = 1,
    /// Production default: aggregate sensing only, no identity-derived fields.
    Anonymous = 2,
    /// Care-home / regulated deployments: class 2 minus risk score and hash.
    Restricted = 3,
}

impl PrivacyClass {
    /// Returns `true` if frames of this class may cross a `NetworkSink`.
    /// Class 0 (`Raw`) is local-only by structural invariant I1.
    #[must_use]
    pub const fn allows_network(self) -> bool {
        !matches!(self, Self::Raw)
    }

    /// Returns `true` if frames of this class may cross the Matter boundary.
    /// Only classes 2 and 3 are Matter-eligible. See ADR-122 §2.4.
    #[must_use]
    pub const fn allows_matter(self) -> bool {
        matches!(self, Self::Anonymous | Self::Restricted)
    }
}

/// Errors produced by BFLD operations.
#[derive(Debug, thiserror::Error)]
pub enum BfldError {
    /// Header magic did not match `BFLD_MAGIC`.
    #[error("invalid BFLD magic: expected 0x{BFLD_MAGIC:08X}, got 0x{0:08X}")]
    InvalidMagic(u32),

    /// Header version unsupported.
    #[error("unsupported BFLD version: {0}")]
    UnsupportedVersion(u16),

    /// Payload CRC32 mismatch — frame corrupted or tampered.
    #[error("payload CRC mismatch: expected 0x{expected:08X}, got 0x{actual:08X}")]
    Crc { expected: u32, actual: u32 },

    /// Attempted to publish a class-0 (`Raw`) frame through a network sink.
    /// Enforces structural invariant I1.
    #[error("privacy violation: {reason}")]
    PrivacyViolation { reason: &'static str },
}
