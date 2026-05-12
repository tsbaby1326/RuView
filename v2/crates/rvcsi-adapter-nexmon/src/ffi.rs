//! Raw FFI to the napi-c shim plus safe wrappers (ADR-096).
//!
//! The C side (`native/rvcsi_nexmon_shim.c`) is allocation-free and bounds-checks
//! every read against the caller-supplied lengths. The `unsafe` here is limited
//! to: calling those C functions with correct pointers/lengths, and reading back
//! the metadata struct the C side fully initialized on `RVCSI_NX_OK`.

use std::os::raw::c_char;

/// Bytes in a record header (the fixed prefix before the I/Q samples).
pub const RECORD_HEADER_BYTES: usize = 24;

/// Largest subcarrier count the shim will parse (mirrors `RVCSI_NX_MAX_SUBCARRIERS`).
pub const MAX_SUBCARRIERS: usize = 2048;

/// Sentinel the C side uses for "metadata field absent".
const ABSENT_I16: i16 = 0x7FFF;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RvcsiNxMeta {
    subcarrier_count: u16,
    channel: u16,
    bandwidth_mhz: u16,
    rssi_dbm: i16,
    noise_floor_dbm: i16,
    timestamp_ns: u64,
}

extern "C" {
    fn rvcsi_nx_record_len(buf: *const u8, len: usize) -> usize;
    fn rvcsi_nx_parse_record(
        buf: *const u8,
        len: usize,
        meta: *mut RvcsiNxMeta,
        i_out: *mut f32,
        q_out: *mut f32,
        cap: usize,
    ) -> i32;
    fn rvcsi_nx_write_record(
        buf: *mut u8,
        cap: usize,
        meta: *const RvcsiNxMeta,
        i_in: *const f32,
        q_in: *const f32,
    ) -> usize;
    fn rvcsi_nx_strerror(code: i32) -> *const c_char;
    fn rvcsi_nx_abi_version() -> u32;
}

/// ABI version of the linked C shim (`major << 16 | minor`).
pub fn shim_abi_version() -> u32 {
    // SAFETY: no arguments, returns a plain u32 by value.
    unsafe { rvcsi_nx_abi_version() }
}

/// Errors decoding a record (a structured view of the C error codes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NexmonFfiError {
    /// The C shim returned a non-zero error code.
    #[error("nexmon shim error {code}: {message}")]
    Shim {
        /// Numeric `RvcsiNxError` code.
        code: i32,
        /// Static description from `rvcsi_nx_strerror`.
        message: String,
    },
    /// The buffer didn't even contain a parseable header / record length.
    #[error("not a record (bad magic, unsupported version, or too short)")]
    NotARecord,
}

fn strerror(code: i32) -> String {
    // SAFETY: rvcsi_nx_strerror always returns a non-NULL pointer to a static,
    // NUL-terminated C string (see the C source); we only borrow it here.
    unsafe {
        let p = rvcsi_nx_strerror(code);
        if p.is_null() {
            return format!("error {code}");
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// A record decoded from the wire: fixed metadata + the I/Q sample vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct NexmonRecord {
    /// Number of subcarriers (== length of `i_values`/`q_values`).
    pub subcarrier_count: u16,
    /// WiFi channel number.
    pub channel: u16,
    /// Bandwidth in MHz.
    pub bandwidth_mhz: u16,
    /// RSSI in dBm, if present in the record.
    pub rssi_dbm: Option<i16>,
    /// Noise floor in dBm, if present.
    pub noise_floor_dbm: Option<i16>,
    /// Source timestamp, ns.
    pub timestamp_ns: u64,
    /// In-phase samples.
    pub i_values: Vec<f32>,
    /// Quadrature samples.
    pub q_values: Vec<f32>,
}

/// Length, in bytes, of the record starting at `buf[0]`, or `None` if `buf`
/// doesn't begin with a complete, valid record.
pub fn record_len(buf: &[u8]) -> Option<usize> {
    // SAFETY: passing a valid pointer + the slice's true length; the C side
    // reads at most `len` bytes and returns 0 on any problem.
    let n = unsafe { rvcsi_nx_record_len(buf.as_ptr(), buf.len()) };
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// Decode the first record in `buf`. Returns the record and the number of bytes
/// it consumed (so callers can advance a cursor over a concatenated stream).
pub fn decode_record(buf: &[u8]) -> Result<(NexmonRecord, usize), NexmonFfiError> {
    let total = record_len(buf).ok_or(NexmonFfiError::NotARecord)?;
    debug_assert!(total >= RECORD_HEADER_BYTES && total <= buf.len());
    let n = (total - RECORD_HEADER_BYTES) / 4;

    let mut meta = RvcsiNxMeta {
        subcarrier_count: 0,
        channel: 0,
        bandwidth_mhz: 0,
        rssi_dbm: 0,
        noise_floor_dbm: 0,
        timestamp_ns: 0,
    };
    let mut i_out = vec![0.0f32; n];
    let mut q_out = vec![0.0f32; n];

    // SAFETY: `buf` is valid for `buf.len()` bytes; `i_out`/`q_out` are valid
    // for `n` f32s each and we pass `n` as the capacity; `meta` points to a
    // fully owned, properly aligned RvcsiNxMeta. The C side writes only within
    // those bounds and fully initializes `meta` on RVCSI_NX_OK.
    let rc = unsafe {
        rvcsi_nx_parse_record(
            buf.as_ptr(),
            buf.len(),
            &mut meta as *mut RvcsiNxMeta,
            i_out.as_mut_ptr(),
            q_out.as_mut_ptr(),
            n,
        )
    };
    if rc != 0 {
        return Err(NexmonFfiError::Shim {
            code: rc,
            message: strerror(rc),
        });
    }
    debug_assert_eq!(meta.subcarrier_count as usize, n);

    let rec = NexmonRecord {
        subcarrier_count: meta.subcarrier_count,
        channel: meta.channel,
        bandwidth_mhz: meta.bandwidth_mhz,
        rssi_dbm: (meta.rssi_dbm != ABSENT_I16).then_some(meta.rssi_dbm),
        noise_floor_dbm: (meta.noise_floor_dbm != ABSENT_I16).then_some(meta.noise_floor_dbm),
        timestamp_ns: meta.timestamp_ns,
        i_values: i_out,
        q_values: q_out,
    };
    Ok((rec, total))
}

/// Encode a record to bytes via the C writer (used by tests and the recorder).
pub fn encode_record(rec: &NexmonRecord) -> Result<Vec<u8>, NexmonFfiError> {
    let n = rec.subcarrier_count as usize;
    if n == 0 || n > MAX_SUBCARRIERS || rec.i_values.len() != n || rec.q_values.len() != n {
        return Err(NexmonFfiError::Shim {
            code: 6,
            message: "bad subcarrier count or i/q length".to_string(),
        });
    }
    let meta = RvcsiNxMeta {
        subcarrier_count: rec.subcarrier_count,
        channel: rec.channel,
        bandwidth_mhz: rec.bandwidth_mhz,
        rssi_dbm: rec.rssi_dbm.unwrap_or(ABSENT_I16),
        noise_floor_dbm: rec.noise_floor_dbm.unwrap_or(ABSENT_I16),
        timestamp_ns: rec.timestamp_ns,
    };
    let cap = RECORD_HEADER_BYTES + n * 4;
    let mut buf = vec![0u8; cap];
    // SAFETY: `buf` is valid for `cap` bytes; `i_in`/`q_in` are valid for `n`
    // f32s each (checked above); `meta` is a fully initialized owned struct.
    let written = unsafe {
        rvcsi_nx_write_record(
            buf.as_mut_ptr(),
            cap,
            &meta as *const RvcsiNxMeta,
            rec.i_values.as_ptr(),
            rec.q_values.as_ptr(),
        )
    };
    if written == 0 {
        return Err(NexmonFfiError::Shim {
            code: 4,
            message: "write_record failed (capacity or argument error)".to_string(),
        });
    }
    debug_assert_eq!(written, cap);
    buf.truncate(written);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_is_not_a_record() {
        assert!(record_len(&[]).is_none());
        assert_eq!(decode_record(&[]).unwrap_err(), NexmonFfiError::NotARecord);
    }

    #[test]
    fn encode_then_decode_is_identity() {
        let rec = NexmonRecord {
            subcarrier_count: 4,
            channel: 11,
            bandwidth_mhz: 20,
            rssi_dbm: Some(-70),
            noise_floor_dbm: None,
            timestamp_ns: 999,
            i_values: vec![1.0, -2.0, 0.0, 3.5],
            q_values: vec![0.5, 0.25, -1.0, 0.0],
        };
        let bytes = encode_record(&rec).unwrap();
        assert_eq!(bytes.len(), RECORD_HEADER_BYTES + 16);
        let (back, consumed) = decode_record(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(back, rec);
    }

    #[test]
    fn rejects_zero_subcarriers_on_encode() {
        let rec = NexmonRecord {
            subcarrier_count: 0,
            channel: 1,
            bandwidth_mhz: 20,
            rssi_dbm: None,
            noise_floor_dbm: None,
            timestamp_ns: 0,
            i_values: vec![],
            q_values: vec![],
        };
        assert!(encode_record(&rec).is_err());
    }
}
