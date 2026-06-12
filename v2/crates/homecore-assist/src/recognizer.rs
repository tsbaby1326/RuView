//! Intent recognizer trait + P1 regex-based implementation.
//!
//! Mirrors `homeassistant.helpers.intent.IntentRecognizer` and the
//! `homeassistant/components/conversation/default_agent.py` regex pattern
//! approach used in HA's classic intent matching.
//!
//! ## P1: `RegexIntentRecognizer`
//!
//! Tries each registered pattern in order; the first match wins.
//! Slot values are extracted from named capture groups.
//!
//! ## `SemanticIntentRecognizer` (real, HNSW-backed)
//!
//! Embeds the utterance with [`crate::embedding`] (deterministic feature
//! hashing) and compares it against a ruvector-core HNSW index of enrolled
//! intent exemplars. When the nearest exemplar's cosine similarity clears a
//! configurable threshold (default `0.75`), its intent is returned with slots
//! extracted by the paired regex pattern. Below threshold it falls back to the
//! regex recognizer. Gated behind the default-on `semantic` feature.

use std::collections::HashMap;

use async_trait::async_trait;
use regex::Regex;
use thiserror::Error;

use crate::intent::{Intent, IntentName};

#[derive(Error, Debug)]
pub enum RecognizerError {
    #[error("regex compile error: {0}")]
    BadPattern(String),
    #[error("recognizer internal error: {0}")]
    Internal(String),
}

/// Core trait every recognizer must implement.
///
/// Returns `Ok(None)` when no intent matches (pipeline falls through to
/// the "not understood" path).
#[async_trait]
pub trait IntentRecognizer: Send + Sync + 'static {
    async fn recognize(
        &self,
        utterance: &str,
        language: &str,
    ) -> Result<Option<Intent>, RecognizerError>;
}

/// A single registered intent pattern.
#[derive(Clone)]
struct IntentPattern {
    name: IntentName,
    /// Pre-compiled regex. Named capture groups become slot keys.
    regex: Regex,
    /// Language tag this pattern applies to. `"*"` means any language.
    language: String,
}

/// P1 recognizer that matches utterances against pre-registered regex patterns.
///
/// Thread-safe: patterns are stored in a `Vec` behind an `Arc<RwLock<_>>` so
/// that `register` can be called from multiple tasks.
#[derive(Clone, Default)]
pub struct RegexIntentRecognizer {
    patterns: std::sync::Arc<tokio::sync::RwLock<Vec<IntentPattern>>>,
}

impl RegexIntentRecognizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a regex pattern for the given intent name and language.
    ///
    /// Named capture groups (e.g. `(?P<entity_id>\w+\.\w+)`) become slot keys.
    /// `language` may be a BCP-47 tag (`"en"`) or `"*"` to match any language.
    ///
    /// # Errors
    ///
    /// Returns `RecognizerError::BadPattern` if the regex fails to compile.
    pub async fn register(
        &self,
        name: impl Into<String>,
        pattern: &str,
        language: impl Into<String>,
    ) -> Result<(), RecognizerError> {
        let regex = Regex::new(pattern).map_err(|e| RecognizerError::BadPattern(e.to_string()))?;
        self.patterns.write().await.push(IntentPattern {
            name: IntentName::new(name),
            regex,
            language: language.into(),
        });
        Ok(())
    }
}

#[async_trait]
impl IntentRecognizer for RegexIntentRecognizer {
    async fn recognize(
        &self,
        utterance: &str,
        language: &str,
    ) -> Result<Option<Intent>, RecognizerError> {
        let normalised = utterance.trim().to_lowercase();
        let patterns = self.patterns.read().await;
        for pattern in patterns.iter() {
            if pattern.language != "*" && pattern.language != language {
                continue;
            }
            if let Some(caps) = pattern.regex.captures(&normalised) {
                let mut slots: HashMap<String, serde_json::Value> = HashMap::new();
                for name in pattern.regex.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        slots.insert(name.to_owned(), serde_json::Value::String(m.as_str().to_owned()));
                    }
                }
                return Ok(Some(Intent {
                    name: pattern.name.clone(),
                    slots,
                    language: language.to_owned(),
                }));
            }
        }
        Ok(None)
    }
}

// `SemanticIntentRecognizer` lives in [`crate::semantic_recognizer`]; this
// module owns only the regex recognizer.

#[cfg(test)]
mod tests {
    use super::*;

    async fn turn_on_recognizer() -> RegexIntentRecognizer {
        let r = RegexIntentRecognizer::new();
        r.register(
            "HassTurnOn",
            r"turn on (?:the )?(?P<entity_id>[a-z_][a-z0-9_ ]*(?:\.[a-z_][a-z0-9_]*)?)",
            "*",
        )
        .await
        .unwrap();
        r.register(
            "HassTurnOff",
            r"turn off (?:the )?(?P<entity_id>[a-z_][a-z0-9_ ]*(?:\.[a-z_][a-z0-9_]*)?)",
            "*",
        )
        .await
        .unwrap();
        r
    }

    #[tokio::test]
    async fn recognizes_turn_on_entity() {
        let r = turn_on_recognizer().await;
        let intent = r
            .recognize("turn on the kitchen light", "en")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(intent.name.as_str(), "HassTurnOn");
        assert!(intent.slots.contains_key("entity_id"));
    }

    #[tokio::test]
    async fn recognizes_dotted_entity_id() {
        let r = turn_on_recognizer().await;
        let intent = r
            .recognize("turn on light.kitchen", "en")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(intent.name.as_str(), "HassTurnOn");
        assert_eq!(intent.entity_id(), Some("light.kitchen"));
    }

    #[tokio::test]
    async fn unrecognized_utterance_returns_none() {
        let r = turn_on_recognizer().await;
        let result = r.recognize("play jazz music", "en").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn language_filter_skips_non_matching() {
        let r = RegexIntentRecognizer::new();
        r.register("HassTurnOn", r"turn on (?P<entity_id>\S+)", "de")
            .await
            .unwrap();
        // German-only pattern must not match an English utterance.
        let result = r.recognize("turn on light.kitchen", "en").await.unwrap();
        assert!(result.is_none());
        // But it must match a German-tagged utterance.
        let result = r.recognize("turn on licht.kueche", "de").await.unwrap();
        assert!(result.is_some());
    }
}
