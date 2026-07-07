//! Minimal HTTP/1.1 server for the UPnP endpoints. One request per
//! connection, `Connection: close` — control points open short-lived
//! connections for SOAP calls, so keep-alive buys nothing here.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::soap::Service;
use crate::{desc, gena, soap, Inner};

const MAX_HEAD: usize = 32 * 1024;
const MAX_BODY: usize = 256 * 1024;

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn xml(status: u16, reason: &'static str, body: String) -> Self {
        let mut r = Self::new(status, reason);
        r.headers
            .push(("Content-Type".into(), "text/xml; charset=\"utf-8\"".into()));
        r.body = body.into_bytes();
        r
    }

    pub fn empty(status: u16, reason: &'static str) -> Self {
        Self::new(status, reason)
    }
}

pub(crate) async fn serve(listener: TcpListener, inner: Arc<Inner>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!(?e, "MediaRenderer accept failed");
                continue;
            }
        };
        let inner = inner.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(30), handle(stream, &inner)).await {
                Ok(Err(e)) => debug!(?e, %peer, "MediaRenderer request failed"),
                Err(_) => debug!(%peer, "MediaRenderer request timed out"),
                Ok(Ok(())) => {}
            }
        });
    }
}

async fn handle(mut stream: TcpStream, inner: &Arc<Inner>) -> Result<()> {
    let req = read_request(&mut stream).await?;
    debug!(method = %req.method, path = %req.path, "mediarenderer request");
    let resp = route(inner, &req).await;
    write_response(&mut stream, &req.method, resp).await
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed mid-request");
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().context("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing method")?.to_uppercase();
    let target = parts.next().context("missing path")?.to_string();
    // Strip any absolute-URI form and query string down to a plain path.
    let path = target
        .split_once("://")
        .map(|(_, rest)| {
            rest.find('/')
                .map(|i| rest[i..].to_string())
                .unwrap_or_else(|| "/".into())
        })
        .unwrap_or(target);
    let path = path.split(['?', '#']).next().unwrap_or("/").to_string();

    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        bail!("request body too large");
    }

    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn write_response(stream: &mut TcpStream, method: &str, resp: Response) -> Result<()> {
    let mut out = format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Server: {}\r\n", Inner::server_header()));
    out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    out.push_str("Connection: close\r\n\r\n");
    stream.write_all(out.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(&resp.body).await?;
    }
    stream.flush().await?;
    Ok(())
}

async fn route(inner: &Arc<Inner>, req: &Request) -> Response {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "HEAD", "/description.xml") => Response::xml(
            200,
            "OK",
            desc::device_description(&inner.opts.friendly_name, &inner.opts.uuid),
        ),
        ("GET" | "HEAD", "/scpd/AVTransport.xml") => {
            Response::xml(200, "OK", desc::av_transport_scpd())
        }
        ("GET" | "HEAD", "/scpd/RenderingControl.xml") => {
            Response::xml(200, "OK", desc::rendering_control_scpd())
        }
        ("GET" | "HEAD", "/scpd/ConnectionManager.xml") => {
            Response::xml(200, "OK", desc::connection_manager_scpd())
        }
        ("POST", path) => match Service::from_control_path(path) {
            Some(svc) => soap::handle(inner, svc, req).await,
            None => Response::empty(404, "Not Found"),
        },
        ("SUBSCRIBE" | "UNSUBSCRIBE", path) => match Service::from_event_path(path) {
            Some(svc) => gena::handle(inner, svc, req),
            None => Response::empty(404, "Not Found"),
        },
        _ => Response::empty(404, "Not Found"),
    }
}
