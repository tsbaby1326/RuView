// rvCSI Node.js SDK — type declarations for the curated `index.js` surface.
//
// The shapes below mirror the Rust `rvcsi-core` schema (`CsiFrame`, `CsiWindow`,
// `CsiEvent`, `SourceHealth`) and `rvcsi-runtime` (`CaptureSummary`). They are
// what you get back after the SDK `JSON.parse`s the strings the napi-rs addon
// returns (see ADR-095 §10 / ADR-096 §2.3).

/** Outcome of the rvCSI validation pipeline for a frame. */
export type ValidationStatus =
  | 'Pending'
  | 'Accepted'
  | 'Degraded'
  | 'Rejected'
  | 'Recovered';

/** Which adapter family produced a frame. */
export type AdapterKind =
  | 'File'
  | 'Replay'
  | 'Nexmon'
  | 'Esp32'
  | 'Intel'
  | 'Atheros'
  | 'Synthetic';

/** Kinds of event the runtime emits. */
export type CsiEventKind =
  | 'PresenceStarted'
  | 'PresenceEnded'
  | 'MotionDetected'
  | 'MotionSettled'
  | 'BaselineChanged'
  | 'SignalQualityDropped'
  | 'DeviceDisconnected'
  | 'BreathingCandidate'
  | 'AnomalyDetected'
  | 'CalibrationRequired';

/** One normalized, validated CSI observation. */
export interface CsiFrame {
  frame_id: number;
  session_id: number;
  source_id: string;
  adapter_kind: AdapterKind;
  timestamp_ns: number;
  channel: number;
  bandwidth_mhz: number;
  rssi_dbm: number | null;
  noise_floor_dbm: number | null;
  antenna_index: number | null;
  tx_chain: number | null;
  rx_chain: number | null;
  subcarrier_count: number;
  i_values: number[];
  q_values: number[];
  amplitude: number[];
  phase: number[];
  validation: ValidationStatus;
  quality_score: number;
  /** Present (non-empty) only when `validation` is `Degraded`. */
  quality_reasons?: string[];
  calibration_version: string | null;
}

/** A bounded window of frames, summarized. */
export interface CsiWindow {
  window_id: number;
  session_id: number;
  source_id: string;
  start_ns: number;
  end_ns: number;
  frame_count: number;
  mean_amplitude: number[];
  phase_variance: number[];
  motion_energy: number;
  presence_score: number;
  quality_score: number;
}

/** A detected event with confidence and the windows that justify it. */
export interface CsiEvent {
  event_id: number;
  kind: CsiEventKind;
  session_id: number;
  source_id: string;
  timestamp_ns: number;
  confidence: number;
  evidence_window_ids: number[];
  calibration_version: string | null;
  /** Free-form JSON string of event metadata. */
  metadata_json: string;
}

/** Health snapshot for a source. */
export interface SourceHealth {
  connected: boolean;
  frames_delivered: number;
  frames_rejected: number;
  status: string | null;
}

/** Per-`ValidationStatus` frame counts. */
export interface ValidationBreakdown {
  pending: number;
  accepted: number;
  degraded: number;
  rejected: number;
  recovered: number;
}

/** Compact summary of a `.rvcsi` capture file. */
export interface CaptureSummary {
  capture_version: number;
  session_id: number;
  source_id: string;
  adapter_kind: string;
  frame_count: number;
  first_timestamp_ns: number;
  last_timestamp_ns: number;
  channels: number[];
  subcarrier_counts: number[];
  mean_quality: number;
  validation_breakdown: ValidationBreakdown;
  calibration_version: string | null;
}

/** rvCSI runtime version string. */
export function rvcsiVersion(): string;

/** ABI version of the linked napi-c Nexmon shim (`major<<16 | minor`). */
export function nexmonShimAbiVersion(): number;

/**
 * Decode a Buffer of "rvCSI Nexmon records" (the napi-c shim format) into
 * validated frames. Throws on a malformed record.
 */
export function nexmonDecodeRecords(
  buf: Buffer | Uint8Array,
  sourceId: string,
  sessionId: number,
): CsiFrame[];

/** Summarize a `.rvcsi` capture file. */
export function inspectCaptureFile(path: string): CaptureSummary;

/** Replay a `.rvcsi` capture through the DSP + event pipeline. */
export function eventsFromCaptureFile(path: string): CsiEvent[];

/** Window a capture and store each window's embedding into a JSONL RF-memory file; returns the count. */
export function exportCaptureToRfMemory(capturePath: string, outJsonlPath: string): number;

/** Streaming capture runtime: a source + the DSP stage + the event pipeline. */
export class RvCsi {
  private constructor(rt: unknown);
  /** Open a `.rvcsi` capture file. */
  static openCaptureFile(path: string): RvCsi;
  /** Open a Nexmon capture file (concatenated rvCSI Nexmon records). */
  static openNexmonFile(path: string, sourceId: string, sessionId: number): RvCsi;
  /** Next exposable, validated frame, or `null` at end-of-stream. */
  nextFrame(): CsiFrame | null;
  /** Like {@link RvCsi.nextFrame} but with the DSP pipeline applied. */
  nextCleanFrame(): CsiFrame | null;
  /** Drain the rest of the stream through DSP + the event pipeline. */
  drainEvents(): CsiEvent[];
  /** Current health snapshot. */
  health(): SourceHealth;
  /** Frames pulled from the source so far. */
  readonly framesSeen: number;
  /** Frames dropped by validation so far. */
  readonly framesDropped: number;
}
