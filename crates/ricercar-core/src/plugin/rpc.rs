//! JSON-RPC 2.0, one JSON object per line, both ways over a pipe pair.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// Longest line read from a plugin; longer ones are dropped.
const MAX_LINE: usize = 32 * 1024 * 1024;
/// Messages waiting for the writer thread; a plugin that stops reading its
/// stdin fills this instead of blocking callers.
const OUT_QUEUE: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Rpc(RpcError),
    Timeout,
    Closed,
    /// The plugin is not reading its input: nothing was sent.
    Busy,
}

/// A message the other side started.
#[derive(Debug)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

type Waiter = Sender<Result<Value, RpcError>>;

pub struct Rpc {
    out: SyncSender<Vec<u8>>,
    next_id: AtomicU64,
    waiting: Arc<Mutex<HashMap<u64, Waiter>>>,
    closed: Arc<AtomicBool>,
}

impl Rpc {
    /// Start reading `input` on a thread; messages started by the other side
    /// go to `incoming` (from that thread: keep it short).
    pub fn start(
        name: &str,
        input: impl Read + Send + 'static,
        output: impl Write + Send + 'static,
        incoming: impl Fn(Incoming) + Send + 'static,
    ) -> Arc<Rpc> {
        let (out, lines) = mpsc::sync_channel::<Vec<u8>>(OUT_QUEUE);
        let rpc = Arc::new(Rpc {
            out,
            next_id: AtomicU64::new(1),
            waiting: Arc::new(Mutex::new(HashMap::new())),
            closed: Arc::new(AtomicBool::new(false)),
        });
        let waiting = rpc.waiting.clone();
        let closed = rpc.closed.clone();
        let label = name.to_string();
        {
            let closed = rpc.closed.clone();
            let mut output = output;
            std::thread::Builder::new()
                .name(format!("ricercar-rpc-{name}-out"))
                .spawn(move || {
                    for line in lines {
                        if output.write_all(&line).is_err() || output.flush().is_err() {
                            closed.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                })
                .expect("spawn rpc writer");
        }
        std::thread::Builder::new()
            .name(format!("ricercar-rpc-{name}"))
            .spawn(move || {
                let mut reader = BufReader::new(input);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match (&mut reader)
                        .take(MAX_LINE as u64 + 1)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if line.len() > MAX_LINE {
                        tracing::warn!("plugin[{label}]: message too long, dropped");
                        // Skip the rest of the line.
                        let mut rest = Vec::new();
                        if reader.read_until(b'\n', &mut rest).is_err() {
                            break;
                        }
                        continue;
                    }
                    let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                        if !line.iter().all(u8::is_ascii_whitespace) {
                            tracing::warn!("plugin[{label}]: invalid JSON line");
                        }
                        continue;
                    };
                    dispatch(msg, &waiting, &incoming);
                }
                closed.store(true, Ordering::SeqCst);
                // Wake every caller still waiting.
                waiting.lock().unwrap().clear();
            })
            .expect("spawn rpc reader");
        rpc
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Queue one message for the writer thread; never blocks.
    fn send(&self, msg: &Value) -> Result<(), CallError> {
        let mut line = msg.to_string().into_bytes();
        line.push(b'\n');
        match self.out.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CallError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(CallError::Closed),
        }
    }

    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, CallError> {
        if self.is_closed() {
            return Err(CallError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.send(&msg) {
            self.waiting.lock().unwrap().remove(&id);
            return Err(e);
        }
        let r = rx.recv_timeout(timeout);
        self.waiting.lock().unwrap().remove(&id);
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(CallError::Rpc(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(CallError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CallError::Closed),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        if !self.is_closed()
            && self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
                == Err(CallError::Busy)
        {
            tracing::debug!("plugin not reading its input: {method} dropped");
        }
    }

    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let msg = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err(e) => {
                let mut err = json!({"code": e.code, "message": e.message});
                if let Some(d) = e.data {
                    err["data"] = d;
                }
                json!({"jsonrpc": "2.0", "id": id, "error": err})
            }
        };
        let _ = self.send(&msg);
    }
}

fn dispatch(msg: Value, waiting: &Mutex<HashMap<u64, Waiter>>, incoming: &impl Fn(Incoming)) {
    if let Some(method) = msg.get("method").and_then(Value::as_str) {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let method = method.to_string();
        match msg.get("id") {
            Some(id) if !id.is_null() => incoming(Incoming::Request {
                id: id.clone(),
                method,
                params,
            }),
            _ => incoming(Incoming::Notification { method, params }),
        }
        return;
    }
    let Some(id) = msg.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(tx) = waiting.lock().unwrap().remove(&id) else {
        return;
    };
    let reply = match (msg.get("result"), msg.get("error")) {
        (_, Some(e)) if !e.is_null() => Err(RpcError {
            code: e.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            data: e.get("data").cloned(),
        }),
        (Some(r), _) => Ok(r.clone()),
        _ => Ok(Value::Null),
    };
    let _ = tx.send(reply);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A peer on the other end of a socket pair that answers `echo`,
    /// fails `boom`, never answers `slow` and sends one request back.
    fn peer() -> (Arc<Rpc>, mpsc::Receiver<Incoming>) {
        let (a, b) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut out = b.try_clone().unwrap();
            writeln!(
                out,
                r#"{{"jsonrpc":"2.0","id":"p1","method":"player.pause","params":{{}}}}"#
            )
            .unwrap();
            for line in BufReader::new(b).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap();
                let Some(id) = v.get("id").cloned() else {
                    continue;
                };
                if v.get("method").is_none() {
                    continue;
                }
                match v["method"].as_str().unwrap() {
                    "echo" => writeln!(out, "{}", json!({"jsonrpc":"2.0","id":id,"result":v["params"]})),
                    "boom" => writeln!(
                        out,
                        "{}",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32004,"message":"slow down","data":{"retry_after":3}}})
                    ),
                    _ => Ok(()),
                }
                .unwrap();
            }
        });
        let (tx, rx) = mpsc::channel();
        let rpc = Rpc::start("test", a.try_clone().unwrap(), a, move |m| {
            let _ = tx.send(m);
        });
        (rpc, rx)
    }

    #[test]
    fn a_peer_that_does_not_read_never_blocks_the_caller() {
        let (a, b) = UnixStream::pair().unwrap();
        let rpc = Rpc::start("stuck", a.try_clone().unwrap(), a, |_| {});
        let t0 = std::time::Instant::now();
        let big = "x".repeat(4096);
        for _ in 0..10_000 {
            rpc.notify("playback.progress", json!({ "pad": big }));
        }
        assert_eq!(
            rpc.call("echo", json!({}), Duration::from_secs(5)),
            Err(CallError::Busy)
        );
        assert!(t0.elapsed() < Duration::from_secs(5));
        drop(b);
    }

    #[test]
    fn calls_errors_timeouts_and_requests() {
        let (rpc, rx) = peer();
        let t = Duration::from_secs(5);
        assert_eq!(rpc.call("echo", json!({"a": 1}), t), Ok(json!({"a": 1})));
        match rpc.call("boom", json!({}), t) {
            Err(CallError::Rpc(e)) => {
                assert_eq!(e.code, -32004);
                assert_eq!(e.data, Some(json!({"retry_after": 3})));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            rpc.call("slow", json!({}), Duration::from_millis(100)),
            Err(CallError::Timeout)
        );
        match rx.recv_timeout(t).unwrap() {
            Incoming::Request { id, method, .. } => {
                assert_eq!((id, method.as_str()), (json!("p1"), "player.pause"))
            }
            other => panic!("{other:?}"),
        }
    }
}
