/*
 * rvCSI — Nexmon CSI compatibility shim (napi-c layer, ADR-095 D2, ADR-096).
 *
 * This is the ONLY C in the rvCSI runtime. It parses (and, for tests, writes)
 * a compact, byte-defined "rvCSI Nexmon record" — a normalized superset of the
 * nexmon_csi UDP payload (magic, RSSI, chanspec, then interleaved int16 I/Q).
 * The Rust side (`rvcsi-adapter-nexmon`) wraps these functions and never sees
 * raw vendor structs; everything above this file is safe Rust.
 *
 * Record layout (all integers little-endian):
 *
 *   off  size  field
 *   ---  ----  -----------------------------------------------------------
 *     0     4  magic            = 0x52564E58  ('R','V','N','X')
 *     4     1  version          = RVCSI_NX_VERSION (1)
 *     5     1  flags            bit0: rssi present, bit1: noise floor present
 *     6     2  subcarrier_count N (1 .. RVCSI_NX_MAX_SUBCARRIERS)
 *     8     1  rssi_dbm         int8 (valid iff flags bit0)
 *     9     1  noise_dbm        int8 (valid iff flags bit1)
 *    10     2  channel          uint16
 *    12     2  bandwidth_mhz    uint16
 *    14     2  reserved         (0)
 *    16     8  timestamp_ns     uint64
 *    24   4*N  N pairs of int16 (i, q), interleaved, fixed-point Q8.8
 *
 *   total record size = 24 + 4*N bytes
 *
 * Fixed-point: stored int16 value v maps to float v / 256.0.
 */
#ifndef RVCSI_NEXMON_SHIM_H
#define RVCSI_NEXMON_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define RVCSI_NX_MAGIC 0x52564E58u /* 'R','V','N','X' little-endian */
#define RVCSI_NX_VERSION 1
#define RVCSI_NX_HEADER_BYTES 24
#define RVCSI_NX_MAX_SUBCARRIERS 2048
#define RVCSI_NX_FLAG_RSSI 0x01u
#define RVCSI_NX_FLAG_NOISE 0x02u

/* Sentinel for "metadata field absent". */
#define RVCSI_NX_ABSENT_I16 ((int16_t)0x7FFF)

/* Error codes returned (negated) by rvcsi_nx_parse_record / rvcsi_nx_write_record. */
typedef enum {
  RVCSI_NX_OK = 0,
  RVCSI_NX_ERR_TOO_SHORT = 1,      /* buffer shorter than the header */
  RVCSI_NX_ERR_BAD_MAGIC = 2,      /* magic mismatch */
  RVCSI_NX_ERR_BAD_VERSION = 3,    /* unsupported version */
  RVCSI_NX_ERR_CAPACITY = 4,       /* caller i/q buffer too small for N */
  RVCSI_NX_ERR_TRUNCATED = 5,      /* buffer shorter than 24 + 4*N */
  RVCSI_NX_ERR_ZERO_SUBCARRIERS = 6,
  RVCSI_NX_ERR_TOO_MANY_SUBCARRIERS = 7,
  RVCSI_NX_ERR_NULL_ARG = 8
} RvcsiNxError;

/* Decoded per-record metadata (the I/Q samples are written separately into
 * caller-provided float arrays). */
typedef struct RvcsiNxMeta {
  uint16_t subcarrier_count;
  uint16_t channel;
  uint16_t bandwidth_mhz;
  int16_t rssi_dbm;       /* RVCSI_NX_ABSENT_I16 if not present */
  int16_t noise_floor_dbm;/* RVCSI_NX_ABSENT_I16 if not present */
  uint64_t timestamp_ns;
} RvcsiNxMeta;

/*
 * Length, in bytes, of the record that starts at `buf`, given `len` bytes are
 * available. Returns 0 if `len` is too small to even read the header, the magic
 * is wrong, the version is unsupported, the subcarrier count is out of range,
 * or `len` < the full record. On success returns 24 + 4*N (>= 28).
 */
size_t rvcsi_nx_record_len(const uint8_t *buf, size_t len);

/*
 * Parse one record at `buf` (with `len` bytes available). Fills `*meta` and
 * writes `subcarrier_count` floats into each of `i_out` and `q_out` (which must
 * each have capacity `cap`). Returns RVCSI_NX_OK (0) on success, or one of the
 * RvcsiNxError codes (positive) on failure. No allocation, no globals.
 */
int rvcsi_nx_parse_record(const uint8_t *buf, size_t len, RvcsiNxMeta *meta,
                          float *i_out, float *q_out, size_t cap);

/*
 * Serialize one record into `buf` (capacity `cap`). `i_in`/`q_in` hold
 * `meta->subcarrier_count` floats each (clamped to the Q8.8 range). Returns the
 * number of bytes written (24 + 4*N) on success, or 0 on error (null arg, zero
 * or too-many subcarriers, capacity too small). Used by Rust tests and the
 * `rvcsi capture` recorder; production capture comes straight off the wire.
 */
size_t rvcsi_nx_write_record(uint8_t *buf, size_t cap, const RvcsiNxMeta *meta,
                             const float *i_in, const float *q_in);

/* Static, human-readable string for an RvcsiNxError code. Never NULL. */
const char *rvcsi_nx_strerror(int code);

/* ABI version of this shim — bumped if the record layout or function
 * signatures change. The Rust side asserts it matches at startup. */
uint32_t rvcsi_nx_abi_version(void);

#ifdef __cplusplus
}
#endif

#endif /* RVCSI_NEXMON_SHIM_H */
