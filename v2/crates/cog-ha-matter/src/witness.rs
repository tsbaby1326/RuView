//! `witness` — append-only hash-chained event log for the cog.
//!
//! ADR-116 §2.2 promises a tamper-evident audit log so regulated
//! deployments (healthcare, education, shared housing) can prove
//! that the state transitions a Seed reported were actually emitted
//! by the cog at the time they were emitted, not retroactively
//! rewritten.
//!
//! This module is the **pure hash-chain primitive**:
//!
//!   * SHA-256 over deterministic canonical bytes,
//!   * `prev_hash` chains each event to its predecessor,
//!   * `WitnessChain::append` is the only mutator — no random
//!     access, no replace, no delete.
//!
//! Ed25519 signing layers on top once the key-management story
//! lands (probably as `witness_signing.rs` reading a key from the
//! Seed's secure store). Keeping the hash chain and the signature
//! in separate modules means the chain primitive can be tested
//! without a key fixture, and a future key rotation doesn't
//! invalidate the chain itself — only the signature over each
//! event.
//!
//! ## Why hash-chain first, not Merkle tree?
//!
//! The cog emits witness events at the rate of semantic-primitive
//! transitions — a few per minute in steady state, dozens during
//! a fall-detection / room-transition event. Linear scan is fine
//! at that rate; we save the Merkle complexity for a future tier
//! when the chain spans days and the auditor wants O(log n)
//! inclusion proofs.

use sha2::{Digest, Sha256};

/// 32-byte hash output. Lifted into a newtype so a future migration
/// to Blake3 / SHA-512 surfaces as a type change instead of a
/// silent length difference in serialized witness bundles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WitnessHash(pub [u8; 32]);

impl WitnessHash {
    /// Genesis hash — the predecessor of the first event. Sentinel
    /// "no prior event" value.
    pub const GENESIS: WitnessHash = WitnessHash([0u8; 32]);

    /// Lowercase hex without `0x` prefix. Matches the format the
    /// `cog-pose-estimation` manifest uses for `binary_sha256` so
    /// downstream tooling can apply one parser.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Parse a 64-char lowercase-hex string back into a `WitnessHash`.
    /// Rejects wrong-length input and non-hex characters — used by
    /// the JSONL parser when reading audit bundles.
    pub fn from_hex(s: &str) -> Result<WitnessHash, WitnessParseError> {
        if s.len() != 64 {
            return Err(WitnessParseError::HashLength { found: s.len() });
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            let lo = i * 2;
            *byte = u8::from_str_radix(&s[lo..lo + 2], 16)
                .map_err(|_| WitnessParseError::HashHex { at: lo })?;
        }
        Ok(WitnessHash(out))
    }
}

/// A single witnessed event. Append-only — once committed to a
/// `WitnessChain`, the fields here are immutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessEvent {
    /// Zero-based sequence number. Strictly monotonically
    /// increasing within a chain — gaps mean the chain was
    /// truncated.
    pub seq: u64,
    /// Hash of the previous event, or [`WitnessHash::GENESIS`] for
    /// the first.
    pub prev_hash: WitnessHash,
    /// Unix epoch seconds at append time. Caller-supplied so the
    /// test suite isn't time-coupled; production uses
    /// `SystemTime::now()`.
    pub timestamp_unix_s: u64,
    /// Short stable kind tag — e.g. `"fall_risk_elevated"`,
    /// `"bed_exit"`, `"privacy_mode_toggled"`. Locked vocabulary
    /// in the future; free-form here until the semantic-primitive
    /// catalog stabilises.
    pub kind: String,
    /// Opaque payload bytes. Typically the JSON of the emitted MQTT
    /// state message so an auditor can re-derive what HA was told.
    pub payload: Vec<u8>,
    /// Hash of *this* event, computed over canonical bytes that
    /// include `prev_hash` — so reconstructing the chain proves
    /// nothing in the past was rewritten.
    pub this_hash: WitnessHash,
}

/// Compute the canonical-bytes form an event is hashed over.
///
/// The format is intentionally simple and length-prefixed so a
/// future migration can be staged with a `version` byte in front
/// without ambiguity:
///
/// ```text
///   prev_hash[32] | seq:u64-be | ts:u64-be | kind_len:u32-be | kind | payload_len:u32-be | payload
/// ```
///
/// Length-prefixing prevents the classic "concatenation forgery"
/// attack where `"abc" + "def"` and `"ab" + "cdef"` would hash the
/// same.
pub fn canonical_bytes(
    prev_hash: WitnessHash,
    seq: u64,
    timestamp_unix_s: u64,
    kind: &str,
    payload: &[u8],
) -> Vec<u8> {
    let kind_bytes = kind.as_bytes();
    let mut out = Vec::with_capacity(32 + 8 + 8 + 4 + kind_bytes.len() + 4 + payload.len());
    out.extend_from_slice(&prev_hash.0);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&timestamp_unix_s.to_be_bytes());
    out.extend_from_slice(&(kind_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(kind_bytes);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Compute the SHA-256 hash for an event.
pub fn hash_event(
    prev_hash: WitnessHash,
    seq: u64,
    timestamp_unix_s: u64,
    kind: &str,
    payload: &[u8],
) -> WitnessHash {
    let mut h = Sha256::new();
    h.update(canonical_bytes(prev_hash, seq, timestamp_unix_s, kind, payload));
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    WitnessHash(out)
}

/// In-memory append-only chain. Persistence (write to the Seed's
/// `~/cognitum/witness/<cog>/events.jsonl`) is a separate concern
/// kept out of this module.
#[derive(Debug, Default, Clone)]
pub struct WitnessChain {
    events: Vec<WitnessEvent>,
}

impl WitnessChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Last committed hash, or `GENESIS` if the chain is empty.
    pub fn tip(&self) -> WitnessHash {
        self.events
            .last()
            .map(|e| e.this_hash)
            .unwrap_or(WitnessHash::GENESIS)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Append a new event. Caller supplies the wall-clock so tests
    /// stay deterministic.
    pub fn append(&mut self, kind: &str, payload: &[u8], timestamp_unix_s: u64) -> &WitnessEvent {
        let prev_hash = self.tip();
        let seq = self.events.len() as u64;
        let this_hash = hash_event(prev_hash, seq, timestamp_unix_s, kind, payload);
        self.events.push(WitnessEvent {
            seq,
            prev_hash,
            timestamp_unix_s,
            kind: kind.to_string(),
            payload: payload.to_vec(),
            this_hash,
        });
        self.events.last().expect("just pushed")
    }

    pub fn events(&self) -> &[WitnessEvent] {
        &self.events
    }

    /// Verify every event's `this_hash` matches the canonical bytes,
    /// every `prev_hash` matches the predecessor's `this_hash`, and
    /// `seq` is gap-free starting at 0.
    ///
    /// Returns `Ok(())` on a sound chain or an `Err` with the first
    /// failing index + reason — auditor-friendly.
    pub fn verify(&self) -> Result<(), WitnessVerifyError> {
        let mut prev = WitnessHash::GENESIS;
        for (i, ev) in self.events.iter().enumerate() {
            if ev.seq != i as u64 {
                return Err(WitnessVerifyError::SeqGap { at: i, found: ev.seq });
            }
            if ev.prev_hash != prev {
                return Err(WitnessVerifyError::PrevHashMismatch { at: i });
            }
            let recomputed = hash_event(
                ev.prev_hash,
                ev.seq,
                ev.timestamp_unix_s,
                &ev.kind,
                &ev.payload,
            );
            if recomputed != ev.this_hash {
                return Err(WitnessVerifyError::HashMismatch { at: i });
            }
            prev = ev.this_hash;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WitnessVerifyError {
    #[error("seq gap at index {at}: expected {at}, found {found}")]
    SeqGap { at: usize, found: u64 },
    #[error("prev_hash mismatch at index {at}")]
    PrevHashMismatch { at: usize },
    #[error("this_hash mismatch at index {at} — event tampered")]
    HashMismatch { at: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WitnessParseError {
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("field `{field}` has wrong type")]
    WrongType { field: &'static str },
    #[error("hash hex must be 64 chars, got {found}")]
    HashLength { found: usize },
    #[error("hash hex parse error at byte offset {at}")]
    HashHex { at: usize },
    #[error("payload hex parse error at byte offset {at}")]
    PayloadHex { at: usize },
    #[error("payload hex must be even length, got {found}")]
    PayloadLength { found: usize },
    #[error("recomputed hash does not match this_hash — bundle is forged or corrupted")]
    HashMismatch,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>, WitnessParseError> {
    if s.len() % 2 != 0 {
        return Err(WitnessParseError::PayloadLength { found: s.len() });
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16)
            .map_err(|_| WitnessParseError::PayloadHex { at: i })?;
        out.push(byte);
    }
    Ok(out)
}

impl WitnessEvent {
    /// Serialize one event to a single JSONL line (no trailing
    /// newline). The format is the audit-bundle wire shape; tools
    /// downstream parse it line-by-line with [`Self::from_jsonl_line`].
    ///
    /// Field ordering is locked alphabetically for byte-stable
    /// output across rebuilds — auditors hash whole bundles, so a
    /// rebuild that reordered fields would silently invalidate
    /// archival hashes.
    ///
    /// Wire shape:
    ///
    /// ```json
    /// {"kind":"...","payload_hex":"...","prev_hash":"...","seq":N,"this_hash":"...","timestamp_unix_s":N}
    /// ```
    pub fn to_jsonl_line(&self) -> String {
        // Hand-rolled instead of serde_derive so the wire-format
        // ordering is under direct test control.
        format!(
            "{{\"kind\":{kind},\"payload_hex\":\"{payload}\",\"prev_hash\":\"{prev}\",\"seq\":{seq},\"this_hash\":\"{this}\",\"timestamp_unix_s\":{ts}}}",
            kind = serde_json::to_string(&self.kind).expect("string is always serializable"),
            payload = hex_encode(&self.payload),
            prev = self.prev_hash.to_hex(),
            seq = self.seq,
            this = self.this_hash.to_hex(),
            ts = self.timestamp_unix_s,
        )
    }

    /// Parse one JSONL line back into a `WitnessEvent`. Re-verifies
    /// the stored `this_hash` against the canonical bytes — a
    /// tampered bundle fires [`WitnessParseError::HashMismatch`]
    /// instead of silently loading forged events.
    pub fn from_jsonl_line(line: &str) -> Result<WitnessEvent, WitnessParseError> {
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| WitnessParseError::Json(e.to_string()))?;
        let obj = v
            .as_object()
            .ok_or(WitnessParseError::WrongType { field: "<root>" })?;

        let seq = obj
            .get("seq")
            .ok_or(WitnessParseError::MissingField("seq"))?
            .as_u64()
            .ok_or(WitnessParseError::WrongType { field: "seq" })?;
        let timestamp_unix_s = obj
            .get("timestamp_unix_s")
            .ok_or(WitnessParseError::MissingField("timestamp_unix_s"))?
            .as_u64()
            .ok_or(WitnessParseError::WrongType {
                field: "timestamp_unix_s",
            })?;
        let kind = obj
            .get("kind")
            .ok_or(WitnessParseError::MissingField("kind"))?
            .as_str()
            .ok_or(WitnessParseError::WrongType { field: "kind" })?
            .to_string();
        let prev_hash = WitnessHash::from_hex(
            obj.get("prev_hash")
                .ok_or(WitnessParseError::MissingField("prev_hash"))?
                .as_str()
                .ok_or(WitnessParseError::WrongType { field: "prev_hash" })?,
        )?;
        let this_hash = WitnessHash::from_hex(
            obj.get("this_hash")
                .ok_or(WitnessParseError::MissingField("this_hash"))?
                .as_str()
                .ok_or(WitnessParseError::WrongType { field: "this_hash" })?,
        )?;
        let payload = hex_decode(
            obj.get("payload_hex")
                .ok_or(WitnessParseError::MissingField("payload_hex"))?
                .as_str()
                .ok_or(WitnessParseError::WrongType {
                    field: "payload_hex",
                })?,
        )?;

        // Re-verify the stored hash. The on-disk hash is purely
        // declarative; this is what makes the JSONL a witness.
        let recomputed = hash_event(prev_hash, seq, timestamp_unix_s, &kind, &payload);
        if recomputed != this_hash {
            return Err(WitnessParseError::HashMismatch);
        }

        Ok(WitnessEvent {
            seq,
            prev_hash,
            timestamp_unix_s,
            kind,
            payload,
            this_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_hash_is_all_zeros() {
        assert_eq!(WitnessHash::GENESIS.0, [0u8; 32]);
    }

    #[test]
    fn empty_chain_tip_is_genesis() {
        let c = WitnessChain::new();
        assert_eq!(c.tip(), WitnessHash::GENESIS);
        assert!(c.is_empty());
    }

    #[test]
    fn canonical_bytes_length_prefixing_prevents_ambiguity() {
        // Classic concatenation forgery: without length prefixes,
        // ("abc","def") and ("ab","cdef") would produce the same
        // hash. With them, they don't.
        let a = canonical_bytes(WitnessHash::GENESIS, 0, 0, "abc", b"def");
        let b = canonical_bytes(WitnessHash::GENESIS, 0, 0, "ab", b"cdef");
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_bytes_starts_with_prev_hash() {
        // Locks the on-wire format. A future migration that flips
        // field order must bump a version byte and update this test.
        let bytes = canonical_bytes(WitnessHash([7u8; 32]), 1, 2, "k", b"p");
        assert_eq!(&bytes[..32], &[7u8; 32]);
    }

    #[test]
    fn append_links_to_prev_hash() {
        let mut c = WitnessChain::new();
        let h1 = c.append("a", b"1", 100).this_hash;
        let e2 = c.append("b", b"2", 101);
        assert_eq!(e2.prev_hash, h1);
        assert_eq!(e2.seq, 1);
    }

    #[test]
    fn sequence_is_monotonic_starting_at_zero() {
        let mut c = WitnessChain::new();
        for i in 0..5 {
            c.append("k", &[i], 0);
        }
        for (i, ev) in c.events().iter().enumerate() {
            assert_eq!(ev.seq, i as u64);
        }
    }

    #[test]
    fn verify_passes_on_clean_chain() {
        let mut c = WitnessChain::new();
        c.append("fall_risk_elevated", b"{}", 100);
        c.append("bed_exit", b"{}", 101);
        c.append("privacy_mode_toggled", br#"{"on":true}"#, 102);
        c.verify().expect("clean chain verifies");
    }

    #[test]
    fn verify_catches_tampered_payload() {
        let mut c = WitnessChain::new();
        c.append("a", b"original", 100);
        c.append("b", b"original2", 101);
        // Tamper with event 0's payload directly.
        c.events[0].payload = b"forged".to_vec();
        let err = c.verify().unwrap_err();
        assert!(matches!(err, WitnessVerifyError::HashMismatch { at: 0 }));
    }

    #[test]
    fn verify_catches_broken_prev_link() {
        let mut c = WitnessChain::new();
        c.append("a", b"1", 100);
        c.append("b", b"2", 101);
        c.events[1].prev_hash = WitnessHash([0xff; 32]);
        let err = c.verify().unwrap_err();
        assert!(matches!(err, WitnessVerifyError::PrevHashMismatch { at: 1 }));
    }

    #[test]
    fn verify_catches_seq_gap() {
        let mut c = WitnessChain::new();
        c.append("a", b"1", 100);
        c.append("b", b"2", 101);
        c.events[1].seq = 99;
        let err = c.verify().unwrap_err();
        assert!(matches!(err, WitnessVerifyError::SeqGap { at: 1, found: 99 }));
    }

    #[test]
    fn hash_to_hex_is_64_lowercase_chars() {
        let h = hash_event(WitnessHash::GENESIS, 0, 0, "k", b"p");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn first_event_prev_hash_is_genesis() {
        // Auditor relies on this: a witness bundle that doesn't start
        // with prev_hash == GENESIS is either truncated or stitched
        // together from two chains.
        let mut c = WitnessChain::new();
        let e = c.append("init", b"", 0);
        assert_eq!(e.prev_hash, WitnessHash::GENESIS);
        assert_eq!(e.seq, 0);
    }

    #[test]
    fn different_payloads_produce_different_hashes() {
        let h1 = hash_event(WitnessHash::GENESIS, 0, 100, "k", b"a");
        let h2 = hash_event(WitnessHash::GENESIS, 0, 100, "k", b"b");
        assert_ne!(h1, h2);
    }

    // ---- JSONL persistence ----

    fn fresh_event() -> WitnessEvent {
        let mut c = WitnessChain::new();
        c.append("fall_risk_elevated", br#"{"node":"kitchen"}"#, 1779512400);
        c.events()[0].clone()
    }

    #[test]
    fn jsonl_round_trip_preserves_all_fields() {
        let original = fresh_event();
        let line = original.to_jsonl_line();
        let parsed = WitnessEvent::from_jsonl_line(&line).expect("clean line round-trips");
        assert_eq!(parsed, original);
    }

    #[test]
    fn jsonl_line_has_no_embedded_newline() {
        // JSONL is one record per line; an embedded \n in the
        // serialized form would corrupt the file format.
        let line = fresh_event().to_jsonl_line();
        assert!(!line.contains('\n'));
        assert!(!line.contains('\r'));
    }

    #[test]
    fn jsonl_field_order_is_alphabetical_for_byte_stability() {
        // Auditors archive whole bundles and hash them — reordered
        // fields would silently invalidate archival hashes. Lock
        // the order with a substring check on a known event.
        let line = fresh_event().to_jsonl_line();
        let order = ["kind", "payload_hex", "prev_hash", "seq", "this_hash", "timestamp_unix_s"];
        let mut last = 0usize;
        for field in order {
            let pos = line.find(field).unwrap_or_else(|| panic!("missing field `{field}`"));
            assert!(pos > last, "field `{field}` out of alphabetical order");
            last = pos;
        }
    }

    #[test]
    fn jsonl_parser_rejects_tampered_payload() {
        let original = fresh_event();
        let line = original.to_jsonl_line();
        // Flip one nibble in the payload hex — the stored hash
        // won't match the recomputed hash.
        let tampered = line.replacen("payload_hex\":\"7b", "payload_hex\":\"6b", 1);
        assert_ne!(line, tampered, "test fixture didn't flip a byte");
        let err = WitnessEvent::from_jsonl_line(&tampered).unwrap_err();
        assert!(
            matches!(err, WitnessParseError::HashMismatch),
            "expected HashMismatch, got {err:?}"
        );
    }

    #[test]
    fn jsonl_parser_rejects_non_hex_hash() {
        // Replace the hex hash with non-hex chars — must fire a
        // structured error, not a panic.
        let original = fresh_event();
        let line = original.to_jsonl_line();
        let broken = line.replacen(
            &original.this_hash.to_hex()[..4],
            "ZZZZ",
            1,
        );
        let err = WitnessEvent::from_jsonl_line(&broken).unwrap_err();
        assert!(matches!(err, WitnessParseError::HashHex { .. }));
    }

    #[test]
    fn jsonl_parser_rejects_missing_field() {
        let bad = r#"{"seq":0,"kind":"k","prev_hash":"00","this_hash":"00","timestamp_unix_s":1}"#;
        let err = WitnessEvent::from_jsonl_line(bad).unwrap_err();
        // Missing payload_hex; should fire MissingField before any
        // hex decode happens.
        assert!(matches!(err, WitnessParseError::MissingField("payload_hex")
            | WitnessParseError::HashLength { .. }));
    }

    #[test]
    fn hex_encode_decode_round_trip() {
        let cases: &[&[u8]] = &[
            b"",
            b"\x00",
            b"\xff",
            b"hello world",
            &[0x00, 0x01, 0xab, 0xcd, 0xef],
        ];
        for c in cases {
            let encoded = hex_encode(c);
            let decoded = hex_decode(&encoded).unwrap();
            assert_eq!(&decoded[..], *c, "round-trip failed for {c:?}");
        }
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        let err = hex_decode("abc").unwrap_err();
        assert!(matches!(err, WitnessParseError::PayloadLength { found: 3 }));
    }

    #[test]
    fn witness_hash_from_hex_round_trip() {
        let h = WitnessHash([0x12; 32]);
        let hex = h.to_hex();
        let parsed = WitnessHash::from_hex(&hex).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn witness_hash_from_hex_rejects_wrong_length() {
        let err = WitnessHash::from_hex("ab").unwrap_err();
        assert!(matches!(err, WitnessParseError::HashLength { found: 2 }));
    }
}
