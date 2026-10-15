use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rmcp::RoleClient;
use rmcp::model::{JsonRpcMessage, ServerRequest};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::JsonRpcMessageCodec;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;

use super::{MAX_FRAME_BYTES, encoded_size};

type StdioWriter = FramedWrite<ChildStdin, JsonRpcMessageCodec<TxJsonRpcMessage<RoleClient>>>;

pub(super) struct StdioTransport {
    read: FramedRead<WireRead, JsonRpcMessageCodec<RxJsonRpcMessage<RoleClient>>>,
    write: Arc<Mutex<Option<StdioWriter>>>,
    stop: CancellationToken,
}

impl StdioTransport {
    pub fn new(stdout: ChildStdout, stdin: ChildStdin, stop: CancellationToken) -> Self {
        Self {
            read: FramedRead::new(
                WireRead {
                    inner: stdout,
                    window: Instant::now(),
                    bytes: 0,
                    messages: 0,
                },
                JsonRpcMessageCodec::new_with_max_length(MAX_FRAME_BYTES),
            ),
            write: Arc::new(Mutex::new(Some(FramedWrite::new(
                stdin,
                JsonRpcMessageCodec::new_with_max_length(MAX_FRAME_BYTES),
            )))),
            stop,
        }
    }
}

impl Transport<RoleClient> for StdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = Arc::clone(&self.write);
        let stop = self.stop.clone();
        async move {
            encoded_size(&item, MAX_FRAME_BYTES)
                .map_err(|_| io::Error::other("MCP outgoing frame exceeded limit"))?;
            let operation = async {
                let mut writer = write.lock().await;
                writer
                    .as_mut()
                    .ok_or_else(|| io::Error::other("MCP transport closed"))?
                    .send(item)
                    .await
                    .map_err(|_| io::Error::other("MCP transport write failed"))
            };
            tokio::select! {
                biased;
                _ = stop.cancelled() => Err(io::Error::other("MCP transport stopped")),
                result = tokio::time::timeout(Duration::from_secs(5), operation) => match result {
                    Ok(Ok(())) => Ok(()),
                    _ => { stop.cancel(); Err(io::Error::other("MCP transport write failed or timed out")) }
                }
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let result = tokio::select! {
            biased;
            _ = self.stop.cancelled() => None,
            frame = self.read.next() => frame,
        };
        match result {
            Some(Ok(message)) => {
                if matches!(&message, JsonRpcMessage::Request(request) if !matches!(&request.request, ServerRequest::PingRequest(_)))
                {
                    self.stop.cancel();
                    return None;
                }
                Some(message)
            }
            _ => {
                self.stop.cancel();
                None
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.stop.cancel();
        self.write.lock().await.take();
        Ok(())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct WireRead {
    inner: ChildStdout,
    window: Instant,
    bytes: usize,
    messages: usize,
}

impl AsyncRead for WireRead {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let state = self.get_mut();
        if state.window.elapsed() >= Duration::from_secs(10) {
            state.window = Instant::now();
            state.bytes = 0;
            state.messages = 0;
        }
        let before = buffer.filled().len();
        let result = Pin::new(&mut state.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &result {
            let read = &buffer.filled()[before..];
            state.bytes = state.bytes.saturating_add(read.len());
            state.messages = state
                .messages
                .saturating_add(read.iter().filter(|byte| **byte == b'\n').count());
            if state.bytes > 8 * MAX_FRAME_BYTES || state.messages > 128 {
                return Poll::Ready(Err(io::Error::other("MCP incoming wire budget exceeded")));
            }
        }
        result
    }
}
