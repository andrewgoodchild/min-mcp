//! A tiny in-process HTTP server for tests.
//!
//! The HTTP-facing clients (`http_upstream`, `oauth`) can only be exercised
//! against a real endpoint, and spawning the binary would put that endpoint in
//! another process — where, under `cargo llvm-cov`, none of it is measured.
//! Running the server on a task in the SAME process keeps the client's code
//! paths in the profile, and makes request counts directly assertable.
//!
//! Deliberately minimal: one handler closure, one port, no routing DSL. The
//! only thing it adds over hyper is the counting and the shutdown.

#![cfg(test)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};

/// What a handler answers with: status, content-type, body.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    /// Extra response headers — `Mcp-Session-Id` is the one that matters here.
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn json(body: impl Into<String>) -> Self {
        Reply { status: 200, content_type: "application/json", body: body.into(), headers: vec![] }
    }
    pub fn sse(body: impl Into<String>) -> Self {
        Reply { status: 200, content_type: "text/event-stream", body: body.into(), headers: vec![] }
    }
    pub fn status(code: u16) -> Self {
        Reply { status: code, content_type: "text/plain", body: String::new(), headers: vec![] }
    }
    pub fn text(body: impl Into<String>) -> Self {
        Reply { status: 200, content_type: "text/plain", body: body.into(), headers: vec![] }
    }
    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

/// One received request, as the test wants to inspect it.
#[derive(Clone, Debug)]
pub struct Received {
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl Received {
    /// First value of a header, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    /// Every request received, in order.
    pub seen: Arc<Mutex<Vec<Received>>>,
    hits: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServer {
    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    pub fn seen(&self) -> Vec<Received> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Start a server on an OS-assigned loopback port. The handler sees the request
/// number (1-based) and body, so a test can answer differently per call — which
/// is how session expiry and token refresh are simulated.
pub async fn spawn<F>(handler: F) -> TestServer
where
    F: Fn(usize, &Received) -> Reply + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    let handler = Arc::new(handler);
    let (h, s) = (hits.clone(), seen.clone());
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = &mut rx => return,
                a = listener.accept() => a,
            };
            let Ok((tcp, _)) = accepted else { continue };
            let (handler, h, s) = (handler.clone(), h.clone(), s.clone());
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let (handler, h, s) = (handler.clone(), h.clone(), s.clone());
                    async move {
                        let headers = req
                            .headers()
                            .iter()
                            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                            .collect();
                        let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                        let got = Received { body: String::from_utf8_lossy(&body).into_owned(), headers };
                        let n = h.fetch_add(1, Ordering::SeqCst) + 1;
                        s.lock().unwrap_or_else(|e| e.into_inner()).push(got.clone());
                        let reply = handler(n, &got);
                        let mut builder =
                            Response::builder().status(reply.status).header("content-type", reply.content_type);
                        for (k, v) in &reply.headers {
                            builder = builder.header(k.as_str(), v.as_str());
                        }
                        let res = builder
                            .body(Full::new(Bytes::from(reply.body)))
                            .expect("build test response");
                        Ok::<_, std::convert::Infallible>(res)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), svc)
                    .await;
            });
        }
    });
    TestServer { addr, seen, hits, shutdown: Some(tx) }
}
