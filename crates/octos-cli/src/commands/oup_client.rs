//! In-process OUP transport for local frontends. The other endpoint is the
//! actual stdio dispatcher used by OctosCode, not a parallel agent runner.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use eyre::{Result, WrapErr};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, WriteHalf};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::api::AppState;
use crate::api::ui_protocol_transport::EmbeddedStdioControl;

type Pending = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

struct PendingRequest {
    pending: Pending,
    id: String,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

pub(crate) struct OupClient {
    control: EmbeddedStdioControl,
    writer: Mutex<Option<WriteHalf<DuplexStream>>>,
    pending: Pending,
    notifications: broadcast::Sender<Value>,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
    server_task: std::sync::Mutex<Option<JoinHandle<Result<()>>>>,
}

pub(crate) struct BackgroundAdmission(Arc<std::sync::atomic::AtomicBool>);
impl Drop for BackgroundAdmission {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl OupClient {
    pub(crate) async fn connect(state: Arc<AppState>) -> Result<Self> {
        let control = EmbeddedStdioControl::default();
        let server_control = control.clone();
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server_task = tokio::spawn(async move {
            crate::api::ui_protocol_transport::embedded_stdio_connection_with_io(
                state,
                server_reader,
                server_writer,
                server_control,
            )
            .await
        });
        let (reader, writer) = tokio::io::split(client_io);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (notifications, _) = broadcast::channel(4096);
        let reader_pending = pending.clone();
        let reader_notifications = notifications.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            let failure = loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break "OUP connection closed".to_owned(),
                    Err(error) => break format!("OUP read failed: {error}"),
                };
                let frame: Value = match serde_json::from_str(&line) {
                    Ok(frame) => frame,
                    Err(error) => break format!("invalid OUP frame: {error}"),
                };
                if let Some(id) = frame.get("id").and_then(Value::as_str) {
                    if let Some(reply) = reader_pending.lock().unwrap().remove(id) {
                        let result = match frame.get("error") {
                            Some(error) => Err(format!("OUP RPC error: {error}")),
                            None => frame.get("result").cloned().ok_or_else(|| {
                                "OUP reply contains neither result nor error".to_owned()
                            }),
                        };
                        let _ = reply.send(result);
                    }
                } else if frame.get("method").is_some() {
                    // A lagged subscriber must recover or fail; broadcast's
                    // explicit Lagged error must never be silently ignored.
                    let _ = reader_notifications.send(frame);
                }
            };
            for (_, reply) in reader_pending.lock().unwrap().drain() {
                let _ = reply.send(Err(failure.clone()));
            }
            let _ = reader_notifications.send(json!({
                "method": "local/connection_closed",
                "params": {"message": failure},
            }));
        });
        Ok(Self {
            control,
            writer: Mutex::new(Some(writer)),
            pending,
            notifications,
            next_id: AtomicU64::new(1),
            reader_task,
            server_task: std::sync::Mutex::new(Some(server_task)),
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifications.subscribe()
    }

    pub(crate) fn allow_background_work(&self) -> BackgroundAdmission {
        self.control
            .continuations_enabled
            .store(true, Ordering::Release);
        BackgroundAdmission(self.control.continuations_enabled.clone())
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.reader_task.is_finished() {
            eyre::bail!("OUP connection is closed");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (reply, receive) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), reply);
        let _pending = PendingRequest {
            pending: self.pending.clone(),
            id: id.clone(),
        };
        let result = async {
            let mut frame = serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params,
            }))?;
            frame.push(b'\n');
            {
                let mut writer = self.writer.lock().await;
                let writer = writer
                    .as_mut()
                    .ok_or_else(|| eyre::eyre!("OUP is closed"))?;
                writer.write_all(&frame).await?;
                writer.flush().await?;
            }
            // This bounds an RPC acknowledgement, not the model turn. Turns
            // finish through notifications and have no client-side time cap.
            receive
                .await
                .wrap_err("OUP reply channel closed")?
                .map_err(|message| eyre::eyre!(message))
        };
        let result = tokio::time::timeout(Duration::from_secs(60), result)
            .await
            .wrap_err_with(|| format!("OUP {method} acknowledgement timed out"));
        result?
    }

    pub(crate) async fn close(&self) -> Result<()> {
        // The dispatcher owns cancellation, durable terminal cleanup, and
        // writer closure. No competing watchdog may abort it halfway through.
        self.control.shutdown.cancel();
        if let Some(mut writer) = self.writer.lock().await.take() {
            let _ = writer.shutdown().await;
        }
        let task = self.server_task.lock().unwrap().take();
        let result = if let Some(task) = task {
            task.await
                .wrap_err("OUP dispatcher task failed")
                .and_then(|result| result)
        } else {
            Ok(())
        };
        self.reader_task.abort();
        result
    }
}

impl Drop for OupClient {
    fn drop(&mut self) {
        self.control.shutdown.cancel();
        self.reader_task.abort();
        // Once the aborted reader drops its half, dropping this half releases
        // the duplex stream and sends EOF. Do not abort the server: its normal
        // teardown must settle active turns first.
        self.writer.get_mut().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embedded_client_uses_real_oup_negotiation_and_rpc_errors() {
        let state = Arc::new(AppState::empty_for_tests());
        let client = OupClient::connect(state).await.unwrap();
        let hello = client
            .request("client_hello", json!({"client": "octos-chat"}))
            .await
            .unwrap();
        assert_eq!(hello["type"], "server_hello");
        assert_eq!(hello["transport"], "stdio");
        let error = client
            .request("nonexistent/method", json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("-32004"), "{error}");
        tokio::time::timeout(std::time::Duration::from_secs(5), client.close())
            .await
            .expect("embedded dispatcher must terminate on EOF")
            .unwrap();
    }
}
