//! MQTT topic router. ADR-122 §2.2.
//!
//! Pure-function module that maps a [`BfldEvent`] into a list of per-entity
//! MQTT topic + payload pairs. No broker dependency lives here — the actual
//! `publish` call is a thin wrapper around `Client::publish(topic, payload)`
//! once a broker integration lands (deferred to a follow-up iter).
//!
//! Topic shape (ADR-122 §2.2):
//!
//! ```text
//! ruview/<node_id>/bfld/presence/state          # class >= 2
//! ruview/<node_id>/bfld/motion/state            # class >= 2
//! ruview/<node_id>/bfld/person_count/state      # class >= 2
//! ruview/<node_id>/bfld/zone_activity/state     # class >= 2 (when zone_id set)
//! ruview/<node_id>/bfld/confidence/state        # class >= 2
//! ruview/<node_id>/bfld/identity_risk/state     # class == 2 only
//! ```
//!
//! `raw` (class-1) and `availability` topics are intentionally not yet emitted
//! by this router; they belong to the broker-connection lifecycle, not to the
//! per-event publish loop.

#![cfg(feature = "std")]

use crate::{BfldEvent, PrivacyClass};

/// Per-topic MQTT message ready to feed into `Client::publish(topic, payload)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicMessage {
    /// Full MQTT topic, e.g. `ruview/seed-01/bfld/presence/state`.
    pub topic: String,
    /// UTF-8 payload bytes — single JSON scalar (`true`, `0.72`, `"living_room"`)
    /// or a compact JSON object for diagnostics.
    pub payload: String,
}

impl TopicMessage {
    /// Build a topic of the form `ruview/<node_id>/bfld/<suffix>/state`.
    #[must_use]
    pub fn ruview_topic(node_id: &str, entity: &str) -> String {
        let mut s = String::with_capacity(7 + node_id.len() + 6 + entity.len() + 6);
        s.push_str("ruview/");
        s.push_str(node_id);
        s.push_str("/bfld/");
        s.push_str(entity);
        s.push_str("/state");
        s
    }
}

/// Render an event into the per-entity MQTT messages it should publish. Returns
/// an empty vec for events that fail the class gate (e.g., raw class 0).
#[must_use]
pub fn render_events(event: &BfldEvent) -> Vec<TopicMessage> {
    let class_byte = event.privacy_class.as_u8();
    if class_byte < PrivacyClass::Anonymous.as_u8() {
        // Raw + Derived stay local — never published on the public topic tree.
        return Vec::new();
    }

    let mut out = Vec::with_capacity(6);
    let node = &event.node_id;

    out.push(TopicMessage {
        topic: TopicMessage::ruview_topic(node, "presence"),
        payload: if event.presence { "true".into() } else { "false".into() },
    });
    out.push(TopicMessage {
        topic: TopicMessage::ruview_topic(node, "motion"),
        payload: format!("{:.6}", event.motion),
    });
    out.push(TopicMessage {
        topic: TopicMessage::ruview_topic(node, "person_count"),
        payload: format!("{}", event.person_count),
    });
    out.push(TopicMessage {
        topic: TopicMessage::ruview_topic(node, "confidence"),
        payload: format!("{:.6}", event.confidence),
    });

    if let Some(zone) = &event.zone_id {
        // Emit a JSON string so consumers can distinguish "no zone" (omitted)
        // from "single-zone deployment" (always the same zone string).
        out.push(TopicMessage {
            topic: TopicMessage::ruview_topic(node, "zone_activity"),
            payload: format!("\"{zone}\""),
        });
    }

    // Identity risk is only published at exactly class 2 (Anonymous). Class 3
    // (Restricted) computes the score internally but never emits it.
    if class_byte == PrivacyClass::Anonymous.as_u8() {
        if let Some(score) = event.identity_risk_score {
            out.push(TopicMessage {
                topic: TopicMessage::ruview_topic(node, "identity_risk"),
                payload: format!("{score:.6}"),
            });
        }
    }

    out
}
