/*
 * rvCSI — Nexmon CSI compatibility shim implementation (napi-c layer).
 * See rvcsi_nexmon_shim.h for the record layout and contract.
 *
 * Deliberately tiny, allocation-free, and dependency-free (libc only). Every
 * read is bounds-checked; nothing here can scribble outside caller buffers.
 */
#include "rvcsi_nexmon_shim.h"

#include <string.h>

#define RVCSI_NX_ABI 0x00010000u /* major.minor = 1.0 */

/* ---- little-endian load/store helpers (portable, no aliasing UB) ---- */

static uint16_t ld_u16(const uint8_t *p) {
  return (uint16_t)((uint16_t)p[0] | ((uint16_t)p[1] << 8));
}
static uint32_t ld_u32(const uint8_t *p) {
  return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
         ((uint32_t)p[3] << 24);
}
static uint64_t ld_u64(const uint8_t *p) {
  return (uint64_t)ld_u32(p) | ((uint64_t)ld_u32(p + 4) << 32);
}
static int16_t ld_i16(const uint8_t *p) { return (int16_t)ld_u16(p); }

static void st_u16(uint8_t *p, uint16_t v) {
  p[0] = (uint8_t)(v & 0xFF);
  p[1] = (uint8_t)((v >> 8) & 0xFF);
}
static void st_u32(uint8_t *p, uint32_t v) {
  p[0] = (uint8_t)(v & 0xFF);
  p[1] = (uint8_t)((v >> 8) & 0xFF);
  p[2] = (uint8_t)((v >> 16) & 0xFF);
  p[3] = (uint8_t)((v >> 24) & 0xFF);
}
static void st_u64(uint8_t *p, uint64_t v) {
  st_u32(p, (uint32_t)(v & 0xFFFFFFFFu));
  st_u32(p + 4, (uint32_t)((v >> 32) & 0xFFFFFFFFu));
}
static void st_i16(uint8_t *p, int16_t v) { st_u16(p, (uint16_t)v); }

/* Q8.8 fixed-point <-> float, with saturation on encode. */
static float q88_to_f(int16_t v) { return (float)v / 256.0f; }
static int16_t f_to_q88(float f) {
  float scaled = f * 256.0f;
  if (scaled >= 32767.0f) return (int16_t)32767;
  if (scaled <= -32768.0f) return (int16_t)-32768;
  /* round to nearest, ties away from zero */
  if (scaled >= 0.0f)
    return (int16_t)(scaled + 0.5f);
  return (int16_t)(scaled - 0.5f);
}

uint32_t rvcsi_nx_abi_version(void) { return RVCSI_NX_ABI; }

const char *rvcsi_nx_strerror(int code) {
  switch (code) {
    case RVCSI_NX_OK: return "ok";
    case RVCSI_NX_ERR_TOO_SHORT: return "buffer too short for header";
    case RVCSI_NX_ERR_BAD_MAGIC: return "bad magic (not an rvCSI Nexmon record)";
    case RVCSI_NX_ERR_BAD_VERSION: return "unsupported record version";
    case RVCSI_NX_ERR_CAPACITY: return "output buffer too small for subcarrier count";
    case RVCSI_NX_ERR_TRUNCATED: return "buffer shorter than the declared record";
    case RVCSI_NX_ERR_ZERO_SUBCARRIERS: return "record declares zero subcarriers";
    case RVCSI_NX_ERR_TOO_MANY_SUBCARRIERS: return "record declares too many subcarriers";
    case RVCSI_NX_ERR_NULL_ARG: return "null argument";
    default: return "unknown error";
  }
}

/* Validate the header at buf[0..24); on success return N (subcarrier count) and
 * the total record size via *out_total. On failure return a negative
 * RvcsiNxError. */
static int validate_header(const uint8_t *buf, size_t len, uint16_t *out_n,
                           size_t *out_total) {
  if (len < (size_t)RVCSI_NX_HEADER_BYTES) return -RVCSI_NX_ERR_TOO_SHORT;
  if (ld_u32(buf) != RVCSI_NX_MAGIC) return -RVCSI_NX_ERR_BAD_MAGIC;
  if (buf[4] != (uint8_t)RVCSI_NX_VERSION) return -RVCSI_NX_ERR_BAD_VERSION;
  uint16_t n = ld_u16(buf + 6);
  if (n == 0) return -RVCSI_NX_ERR_ZERO_SUBCARRIERS;
  if (n > RVCSI_NX_MAX_SUBCARRIERS) return -RVCSI_NX_ERR_TOO_MANY_SUBCARRIERS;
  size_t total = (size_t)RVCSI_NX_HEADER_BYTES + (size_t)n * 4u;
  if (len < total) return -RVCSI_NX_ERR_TRUNCATED;
  *out_n = n;
  *out_total = total;
  return 0;
}

size_t rvcsi_nx_record_len(const uint8_t *buf, size_t len) {
  if (buf == NULL) return 0;
  uint16_t n;
  size_t total;
  if (validate_header(buf, len, &n, &total) < 0) return 0;
  return total;
}

int rvcsi_nx_parse_record(const uint8_t *buf, size_t len, RvcsiNxMeta *meta,
                          float *i_out, float *q_out, size_t cap) {
  if (buf == NULL || meta == NULL || i_out == NULL || q_out == NULL)
    return RVCSI_NX_ERR_NULL_ARG;

  uint16_t n;
  size_t total;
  int rc = validate_header(buf, len, &n, &total);
  if (rc < 0) return -rc;
  if ((size_t)n > cap) return RVCSI_NX_ERR_CAPACITY;

  uint8_t flags = buf[5];
  meta->subcarrier_count = n;
  meta->channel = ld_u16(buf + 10);
  meta->bandwidth_mhz = ld_u16(buf + 12);
  meta->rssi_dbm =
      (flags & RVCSI_NX_FLAG_RSSI) ? (int16_t)(int8_t)buf[8] : RVCSI_NX_ABSENT_I16;
  meta->noise_floor_dbm =
      (flags & RVCSI_NX_FLAG_NOISE) ? (int16_t)(int8_t)buf[9] : RVCSI_NX_ABSENT_I16;
  meta->timestamp_ns = ld_u64(buf + 16);

  const uint8_t *p = buf + RVCSI_NX_HEADER_BYTES;
  for (uint16_t k = 0; k < n; ++k) {
    i_out[k] = q88_to_f(ld_i16(p));
    q_out[k] = q88_to_f(ld_i16(p + 2));
    p += 4;
  }
  return RVCSI_NX_OK;
}

size_t rvcsi_nx_write_record(uint8_t *buf, size_t cap, const RvcsiNxMeta *meta,
                             const float *i_in, const float *q_in) {
  if (buf == NULL || meta == NULL || i_in == NULL || q_in == NULL) return 0;
  uint16_t n = meta->subcarrier_count;
  if (n == 0 || n > RVCSI_NX_MAX_SUBCARRIERS) return 0;
  size_t total = (size_t)RVCSI_NX_HEADER_BYTES + (size_t)n * 4u;
  if (cap < total) return 0;

  memset(buf, 0, RVCSI_NX_HEADER_BYTES);
  st_u32(buf, RVCSI_NX_MAGIC);
  buf[4] = (uint8_t)RVCSI_NX_VERSION;
  uint8_t flags = 0;
  if (meta->rssi_dbm != RVCSI_NX_ABSENT_I16) flags |= RVCSI_NX_FLAG_RSSI;
  if (meta->noise_floor_dbm != RVCSI_NX_ABSENT_I16) flags |= RVCSI_NX_FLAG_NOISE;
  buf[5] = flags;
  st_u16(buf + 6, n);
  buf[8] = (uint8_t)(int8_t)((flags & RVCSI_NX_FLAG_RSSI) ? meta->rssi_dbm : 0);
  buf[9] = (uint8_t)(int8_t)((flags & RVCSI_NX_FLAG_NOISE) ? meta->noise_floor_dbm : 0);
  st_u16(buf + 10, meta->channel);
  st_u16(buf + 12, meta->bandwidth_mhz);
  st_u16(buf + 14, 0);
  st_u64(buf + 16, meta->timestamp_ns);

  uint8_t *p = buf + RVCSI_NX_HEADER_BYTES;
  for (uint16_t k = 0; k < n; ++k) {
    st_i16(p, f_to_q88(i_in[k]));
    st_i16(p + 2, f_to_q88(q_in[k]));
    p += 4;
  }
  return total;
}
