//! Wire protocol between the CLI and the local inference daemon.
//!
//! Length-prefixed JSON frames over a unix socket: a 4-byte big-endian length,
//! then that many bytes of JSON. Framing is explicit rather than newline
//! delimited because message bodies contain newlines and we would otherwise be
//! escaping them.
//!
//! Everything here derives serde, including the goose and rmcp types passed
//! straight through — those already round-trip, so the daemon boundary needs no
//! translation layer.

use anyhow::{bail, Context, Result};
use goose_providers::conversation::message::Message;
use rmcp::model::Tool;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bumped when a change would make an old daemon misinterpret a new request.
/// The binary version is checked separately and is the stricter of the two.
pub const VERSION: u32 = 1;

/// A frame larger than this means a bug or a hostile peer, not a real request.
const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Run one inference.
    Complete(Box<CompleteRequest>),
    /// Liveness, version, and what the daemon is holding.
    Status,
    /// Exit once the in-flight request finishes.
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Completed {
        message: Box<Message>,
        input_tokens: Option<i32>,
        output_tokens: Option<i32>,
    },
    Status(Status),
    /// Acknowledges `Shutdown`.
    ShuttingDown,
    /// The daemon reached the model and it failed. The message is rendered for
    /// the user, so the CLI prints it as-is rather than re-wrapping it.
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// Binary version the daemon was built from. A mismatch means the CLI was
    /// upgraded underneath it.
    pub binary_version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub started_at: String,
    pub requests_served: u64,
    /// Models loaded since start, in load order.
    pub models: Vec<String>,
}

/// A framed message, tagged with the protocol version so a mismatch is a clear
/// error rather than a confusing deserialisation failure.
#[derive(Debug, Serialize, Deserialize)]
pub struct Frame<T> {
    pub v: u32,
    pub body: T,
}

impl<T> Frame<T> {
    pub fn new(body: T) -> Self {
        Self { v: VERSION, body }
    }
}

pub async fn write_frame<W, T>(writer: &mut W, body: T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(&Frame::new(body))?;
    let length = u32::try_from(bytes.len()).context("frame too large to encode")?;
    if length > MAX_FRAME {
        bail!("frame of {length} bytes exceeds the {MAX_FRAME} byte limit");
    }
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut length = [0u8; 4];
    reader
        .read_exact(&mut length)
        .await
        .context("connection closed before a frame arrived")?;
    let length = u32::from_be_bytes(length);
    if length > MAX_FRAME {
        bail!("peer announced a {length} byte frame, over the limit");
    }

    let mut bytes = vec![0u8; length as usize];
    reader
        .read_exact(&mut bytes)
        .await
        .context("connection closed mid-frame")?;

    let frame: Frame<T> = serde_json::from_slice(&bytes).context("malformed frame")?;
    if frame.v != VERSION {
        bail!(
            "protocol version mismatch: peer speaks v{}, this binary speaks v{VERSION}",
            frame.v
        );
    }
    Ok(frame.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_round_trips() {
        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            Request::Complete(Box::new(CompleteRequest {
                model: "some/model:Q4".into(),
                system: "be helpful".into(),
                messages: vec![Message::user().with_text("hello")],
                tools: Vec::new(),
            })),
        )
        .await
        .unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        let request: Request = read_frame(&mut cursor).await.unwrap();
        match request {
            Request::Complete(complete) => {
                assert_eq!(complete.model, "some/model:Q4");
                assert_eq!(complete.messages.len(), 1);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_response_round_trips() {
        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            Response::Completed {
                message: Box::new(Message::assistant().with_text("hi")),
                input_tokens: Some(10),
                output_tokens: Some(4),
            },
        )
        .await
        .unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        match read_frame::<_, Response>(&mut cursor).await.unwrap() {
            Response::Completed {
                message,
                input_tokens,
                ..
            } => {
                assert_eq!(message.as_concat_text(), "hi");
                assert_eq!(input_tokens, Some(10));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Bodies contain newlines, so framing has to be length-prefixed. Two
    /// frames back to back must not bleed into each other.
    #[tokio::test]
    async fn frames_do_not_run_together() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, Request::Status).await.unwrap();
        write_frame(
            &mut buffer,
            Request::Complete(Box::new(CompleteRequest {
                model: "m".into(),
                system: "multi\nline\nsystem".into(),
                messages: vec![Message::user().with_text("also\nmultiline")],
                tools: Vec::new(),
            })),
        )
        .await
        .unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        assert!(matches!(
            read_frame::<_, Request>(&mut cursor).await.unwrap(),
            Request::Status
        ));
        match read_frame::<_, Request>(&mut cursor).await.unwrap() {
            Request::Complete(complete) => assert_eq!(complete.system, "multi\nline\nsystem"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_version_mismatch_is_a_clear_error() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "v": 99,
            "body": {"op": "status"}
        }))
        .unwrap();
        let mut framed = (bytes.len() as u32).to_be_bytes().to_vec();
        framed.extend(bytes);

        let mut cursor = std::io::Cursor::new(framed);
        let error = read_frame::<_, Request>(&mut cursor)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("version mismatch"), "{error}");
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error_not_a_hang() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 100, 1, 2, 3]);
        let error = read_frame::<_, Request>(&mut cursor)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("mid-frame"), "{error}");
    }
}
