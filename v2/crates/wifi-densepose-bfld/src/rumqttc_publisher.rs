//! `RumqttPublisher` — production [`Publish`] impl backed by `rumqttc`.
//! ADR-122 §2.2 broker integration.
//!
//! Gated on `feature = "mqtt"`. The sync `rumqttc::Client` is used so the
//! `Publish` trait's sync method signature is honored without a tokio runtime.
//! The companion `rumqttc::Connection` returned by [`RumqttPublisher::connect`]
//! must be pumped by the caller (typically on a dedicated thread) to drive
//! the MQTT protocol — published messages remain queued until the connection
//! sends them.
//!
//! ```ignore
//! use std::thread;
//! use wifi_densepose_bfld::{publish_event, RumqttPublisher};
//! use rumqttc::MqttOptions;
//!
//! let opts = MqttOptions::new("seed-01", "broker.local", 1883);
//! let (mut publisher, mut connection) = RumqttPublisher::connect(opts, 100);
//! thread::spawn(move || for _ in connection.iter() { /* drain */ });
//! // ... build BfldEvent ...
//! publish_event(&mut publisher, &event).expect("mqtt publish");
//! ```

#![cfg(feature = "mqtt")]

use rumqttc::{Client, Connection, MqttOptions, QoS};

use crate::mqtt_topics::{Publish, TopicMessage};

/// Sync MQTT publisher wrapping [`rumqttc::Client`].
pub struct RumqttPublisher {
    client: Client,
    qos: QoS,
    retain: bool,
}

impl RumqttPublisher {
    /// Wrap an existing `Client` at the supplied QoS. `retain = false` matches
    /// HA-DISCO state-topic semantics (retained payloads cause stale-state
    /// flapping on broker reconnect). For availability-style topics callers
    /// should construct a separate publisher with `retain = true`.
    #[must_use]
    pub const fn new(client: Client, qos: QoS) -> Self {
        Self {
            client,
            qos,
            retain: false,
        }
    }

    /// Toggle the per-publisher `retain` flag.
    #[must_use]
    pub const fn with_retain(mut self, retain: bool) -> Self {
        self.retain = retain;
        self
    }

    /// Build a publisher + an unpumped `Connection`. Caller is responsible
    /// for spawning a thread that iterates the connection (typical pattern
    /// shown in the module-level doc example).
    #[must_use]
    pub fn connect(opts: MqttOptions, capacity: usize) -> (Self, Connection) {
        let (client, connection) = Client::new(opts, capacity);
        (Self::new(client, QoS::AtLeastOnce), connection)
    }
}

impl Publish for RumqttPublisher {
    type Error = rumqttc::ClientError;

    fn publish(&mut self, msg: &TopicMessage) -> Result<(), Self::Error> {
        self.client
            .publish(&msg.topic, self.qos, self.retain, msg.payload.as_bytes())
    }
}
