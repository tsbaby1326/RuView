//! Bridge between sensing-server frame data and signal crate FieldModel
//! for eigenvalue-based person counting.
//!
//! The FieldModel decomposes CSI observations into environmental drift and
//! body perturbation via SVD eigenmodes. When calibrated, perturbation energy
//! provides a physics-grounded occupancy estimate that supplements the
//! score-based heuristic in `score_to_person_count`.

use std::collections::VecDeque;
use wifi_densepose_signal::ruvsense::field_model::{CalibrationStatus, FieldModel};

use super::score_to_person_count;

/// Number of recent frames to feed into perturbation extraction.
const OCCUPANCY_WINDOW: usize = 50;

/// Perturbation energy threshold for detecting a second person.
const ENERGY_THRESH_2: f64 = 12.0;
/// Perturbation energy threshold for detecting a third person.
const ENERGY_THRESH_3: f64 = 25.0;

/// Estimate occupancy using the FieldModel when calibrated, falling back
/// to the score-based heuristic otherwise.
///
/// When the field model is `Fresh` or `Stale`, we extract body perturbation
/// from the most recent frames and map total energy to a person count.
/// On any error or when uncalibrated, we fall through to `score_to_person_count`.
pub fn occupancy_or_fallback(
    field: &FieldModel,
    frame_history: &VecDeque<Vec<f64>>,
    smoothed_score: f64,
    prev_count: usize,
) -> usize {
    match field.status() {
        CalibrationStatus::Fresh | CalibrationStatus::Stale => {
            let frames: Vec<Vec<f64>> = frame_history
                .iter()
                .rev()
                .take(OCCUPANCY_WINDOW)
                .cloned()
                .collect();

            if frames.is_empty() {
                return score_to_person_count(smoothed_score, prev_count);
            }

            // Use the most recent frame as the observation for perturbation
            // extraction. The FieldModel expects [n_links][n_subcarriers],
            // so we wrap the single frame as a single-link observation.
            let observation = vec![frames[0].clone()];
            match field.extract_perturbation(&observation) {
                Ok(perturbation) => {
                    if perturbation.total_energy > ENERGY_THRESH_3 {
                        3
                    } else if perturbation.total_energy > ENERGY_THRESH_2 {
                        2
                    } else {
                        1
                    }
                }
                Err(e) => {
                    tracing::warn!("FieldModel perturbation failed, using fallback: {e}");
                    score_to_person_count(smoothed_score, prev_count)
                }
            }
        }
        _ => score_to_person_count(smoothed_score, prev_count),
    }
}

/// Feed the latest frame to the FieldModel during calibration collection.
///
/// Only acts when the model status is `Collecting`. Wraps the latest frame
/// as a single-link observation and feeds it; errors are logged and ignored.
pub fn maybe_feed_calibration(field: &mut FieldModel, frame_history: &VecDeque<Vec<f64>>) {
    if field.status() != CalibrationStatus::Collecting {
        return;
    }
    if let Some(latest) = frame_history.back() {
        let observations = vec![latest.clone()];
        if let Err(e) = field.feed_calibration(&observations) {
            tracing::warn!("FieldModel calibration feed error: {e}");
        }
    }
}

/// Parse node positions from a semicolon-delimited string.
///
/// Format: `"x,y,z;x,y,z;..."` where each coordinate is an `f32`.
/// Entries that fail to parse are silently skipped.
pub fn parse_node_positions(input: &str) -> Vec<[f32; 3]> {
    if input.is_empty() {
        return Vec::new();
    }
    input
        .split(';')
        .filter_map(|triplet| {
            let parts: Vec<&str> = triplet.split(',').collect();
            if parts.len() != 3 {
                return None;
            }
            let x = parts[0].parse::<f32>().ok()?;
            let y = parts[1].parse::<f32>().ok()?;
            let z = parts[2].parse::<f32>().ok()?;
            Some([x, y, z])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_node_positions() {
        let positions = parse_node_positions("0,0,1.5;3,0,1.5;1.5,3,1.5");
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0], [0.0, 0.0, 1.5]);
        assert_eq!(positions[1], [3.0, 0.0, 1.5]);
        assert_eq!(positions[2], [1.5, 3.0, 1.5]);
    }

    #[test]
    fn test_parse_node_positions_empty() {
        let positions = parse_node_positions("");
        assert!(positions.is_empty());
    }

    #[test]
    fn test_parse_node_positions_invalid() {
        let positions = parse_node_positions("abc;1,2,3");
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0], [1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_parse_node_positions_partial_triplet() {
        let positions = parse_node_positions("1,2;3,4,5");
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0], [3.0, 4.0, 5.0]);
    }
}
