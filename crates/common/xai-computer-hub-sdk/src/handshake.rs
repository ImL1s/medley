//! Hello handshake helpers used by the connection actor and the
//! reconnect-replay path.
//!
//! Splitting these into a dedicated module keeps the connection state
//! machine readable: send the frame, parse the ack, surface a typed
//! [`crate::ClientError`].

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use xai_tool_protocol::{ConnectionKind, HelloAckMsg, HelloMsg};

use crate::error::ClientError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandshakeCloseMetadata {
    code: Option<u16>,
    reason_present: bool,
    reason_len: usize,
}

impl HandshakeCloseMetadata {
    fn from_frame(frame: Option<&tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> Self {
        match frame {
            Some(frame) => Self {
                code: Some(frame.code.into()),
                reason_present: !frame.reason.is_empty(),
                reason_len: frame.reason.len(),
            },
            None => Self {
                code: None,
                reason_present: false,
                reason_len: 0,
            },
        }
    }
}

impl std::fmt::Display for HandshakeCloseMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "code={}; reason_present={}; reason_len={}",
            self.code
                .map_or_else(|| "none".to_owned(), |code| code.to_string()),
            self.reason_present,
            self.reason_len
        )
    }
}

/// Wire-protocol version both ends speak. Re-exported from the
/// protocol crate so the SDK and the IC service share one source of
/// truth.
pub use xai_tool_protocol::PROTOCOL_VERSION;

/// Send the [`HelloMsg`] and wait for the matching [`HelloAckMsg`].
///
/// `kind` should be [`ConnectionKind::ToolServer`] for tool-server
/// builds (the only consumer today). The function returns the parsed
/// ack so callers can observe the server-issued `connection_id` and
/// the server-derived `user_id`.
///
/// When `server_id` is `Some`, it is included in the hello frame so the
/// server can identify itself without a separate `register_server` call.
pub async fn send_hello<Si, St>(
    sink: &mut Si,
    stream: &mut St,
    kind: ConnectionKind,
    server_id: Option<xai_tool_protocol::ServerId>,
    description: Option<String>,
    metadata: Option<serde_json::Value>,
) -> Result<HelloAckMsg, ClientError>
where
    Si: SinkExt<Message> + Unpin,
    Si::Error: std::fmt::Display,
    St: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let hello = HelloMsg {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        kind,
        server_id,
        description,
        metadata,
    };
    let text = serde_json::to_string(&hello)?;
    sink.send(Message::Text(text.into()))
        .await
        .map_err(|_| ClientError::NetworkError("hello send failed".to_owned()))?;

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        match msg {
            Message::Text(text) => {
                let ack: HelloAckMsg = serde_json::from_str(text.as_ref())
                    .map_err(|e| ClientError::ProtocolError(format!("malformed hello_ack: {e}")))?;
                if !ack
                    .supported_protocol_versions
                    .iter()
                    .any(|v| v == PROTOCOL_VERSION)
                {
                    return Err(ClientError::ProtocolError(format!(
                        "server does not support {PROTOCOL_VERSION}; supported: {:?}",
                        ack.supported_protocol_versions
                    )));
                }
                return Ok(ack);
            }
            Message::Ping(payload) => {
                sink.send(Message::Pong(payload))
                    .await
                    .map_err(|_| ClientError::NetworkError("pong send failed".to_owned()))?;
            }
            Message::Close(frame) => {
                let metadata = HandshakeCloseMetadata::from_frame(frame.as_ref());
                return Err(ClientError::Closed(format!(
                    "server closed during handshake ({metadata})"
                )));
            }
            Message::Pong(_) | Message::Frame(_) => continue,
            Message::Binary(_) => {
                return Err(ClientError::ProtocolError(
                    "server sent binary frame during handshake".to_owned(),
                ));
            }
        }
    }
    Err(ClientError::NetworkError(
        "server closed before hello_ack".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::{sink, stream};
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    use super::*;

    const SECRET_SENTINEL: &str = "ZXQ91vLmN7pR4tK8sW2cY6hF0aD3uB5e";

    fn assert_no_secret_fragments(rendered: &str, secret: &str) {
        assert!(!rendered.contains(secret), "secret leaked: {rendered}");
        for window in secret.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !rendered.contains(fragment),
                "secret fragment {fragment:?} leaked: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn peer_close_reason_is_reduced_to_typed_metadata() {
        let frame = CloseFrame {
            code: CloseCode::Policy,
            reason: SECRET_SENTINEL.into(),
        };
        let mut sink = sink::drain::<Message>().sink_map_err(|never: Infallible| match never {});
        let mut stream = stream::iter([Ok(Message::Close(Some(frame)))]);

        let error = send_hello(
            &mut sink,
            &mut stream,
            ConnectionKind::ToolServer,
            None,
            None,
            None,
        )
        .await
        .expect_err("peer close must fail the handshake");

        let rendered = error.to_string();
        assert_no_secret_fragments(&rendered, SECRET_SENTINEL);
        assert!(rendered.contains("code=1008"));
        assert!(rendered.contains("reason_present=true"));
        assert!(rendered.contains(&format!("reason_len={}", SECRET_SENTINEL.len())));
    }
}
