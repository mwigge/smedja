use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::{Request, Response};

/// Length-prefixed newline-delimited JSON framing over a Unix socket.
pub struct Codec {
    stream: BufReader<UnixStream>,
}

impl Codec {
    pub fn new(stream: UnixStream) -> Self {
        Self { stream: BufReader::new(stream) }
    }

    pub async fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.stream.get_mut().write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Option<Value>> {
        let mut line = String::new();
        let n = self.stream.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(line.trim_end())?))
    }

    pub async fn call(&mut self, req: &Request) -> Result<Response> {
        self.send(req).await?;
        let raw = self.recv().await?.ok_or_else(|| anyhow::anyhow!("connection closed"))?;
        Ok(serde_json::from_value(raw)?)
    }
}
