//! 401 attribution: callback hook + shared helpers for tool HTTP clients.

use std::sync::Arc;
use xai_grok_auth::CredentialComparison;

/// Which tool endpoint produced the 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConsumer {
    ImageGen,
    VideoGenStart,
    VideoGenPoll,
    WebSearch,
}

impl ToolConsumer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageGen => "ImageGen",
            Self::VideoGenStart => "VideoGen.start",
            Self::VideoGenPoll => "VideoGen.poll",
            Self::WebSearch => "WebSearch",
        }
    }
}

/// 401 attribution callback. Shell wires this to emit telemetry.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    fn record_401(&self, consumer: ToolConsumer, comparison: CredentialComparison);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;

pub(crate) fn emit_401(
    callback: Option<&SharedAttributionCallback>,
    consumer: ToolConsumer,
    comparison: CredentialComparison,
) {
    if let Some(cb) = callback {
        cb.record_401(consumer, comparison);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_consumer_as_str_stable_identifiers() {
        assert_eq!(ToolConsumer::ImageGen.as_str(), "ImageGen");
        assert_eq!(ToolConsumer::VideoGenStart.as_str(), "VideoGen.start");
        assert_eq!(ToolConsumer::VideoGenPoll.as_str(), "VideoGen.poll");
        assert_eq!(ToolConsumer::WebSearch.as_str(), "WebSearch");
    }
}
