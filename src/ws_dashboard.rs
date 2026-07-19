use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};
use tracing::{info, warn};

use crate::state::now_ms;

const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const DASHBOARD_SESSION_COOKIE: &str = "polytread_dashboard_session";

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradeSide {
    BuyUp,
    BuyDown,
    SellUp,
    SellDown,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    Taker,
    Maker,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type", content = "payload")]
pub enum DashboardCmd {
    SetEnabled {
        enabled: bool,
    },
    SubmitOrder {
        side: TradeSide,
        nominal_usd: f64,
        mechanism: Mechanism,
        expected_session_slug: String,
    },
    ClaimPosition {
        condition_id: String,
        expected_redeemable_value_usd: f64,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DashboardCommandEnvelope {
    pub request_id: String,
    pub command: DashboardCmd,
}

#[derive(Debug, Clone)]
pub struct DashboardCommand {
    pub command: DashboardCmd,
    pub request_id: String,
    pub dashboard_received_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardControl {
    Shutdown,
}

fn parse_dashboard_command(body: &str) -> anyhow::Result<DashboardCommand> {
    let envelope = serde_json::from_str::<DashboardCommandEnvelope>(body)?;
    if envelope.request_id.trim().is_empty() || envelope.request_id.len() > 128 {
        anyhow::bail!("request_id must contain 1 to 128 characters");
    }
    Ok(DashboardCommand {
        command: envelope.command,
        request_id: envelope.request_id,
        dashboard_received_at_ms: now_ms(),
    })
}

pub struct DashboardServer {
    snapshot_tx: broadcast::Sender<serde_json::Value>,
    command_tx: mpsc::Sender<DashboardCommand>,
    control_tx: mpsc::Sender<DashboardControl>,
    control_token: Option<Arc<str>>,
    dashboard_token: Arc<str>,
}

struct ConnectionContext {
    command_tx: mpsc::Sender<DashboardCommand>,
    control_tx: mpsc::Sender<DashboardControl>,
    control_token: Option<Arc<str>>,
    dashboard_token: Arc<str>,
    snapshot_rx: broadcast::Receiver<serde_json::Value>,
    latest: Arc<RwLock<Option<String>>>,
    command_ids: Arc<Mutex<HashMap<String, i64>>>,
}

impl DashboardServer {
    pub fn new(
        snapshot_tx: broadcast::Sender<serde_json::Value>,
        control_token: Option<String>,
        dashboard_token: String,
    ) -> (
        Self,
        mpsc::Receiver<DashboardCommand>,
        mpsc::Receiver<DashboardControl>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(128);
        let (control_tx, control_rx) = mpsc::channel(4);
        (
            Self {
                snapshot_tx,
                command_tx,
                control_tx,
                control_token: control_token.map(Arc::<str>::from),
                dashboard_token: Arc::<str>::from(dashboard_token),
            },
            command_rx,
            control_rx,
        )
    }

    pub async fn run(
        self,
        bind_addr: &str,
        ready_tx: oneshot::Sender<SocketAddr>,
    ) -> anyhow::Result<()> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        info!(%local_addr, "PolyTread browser service listening");
        let _ = ready_tx.send(local_addr);

        let latest = Arc::new(RwLock::new(None::<String>));
        let latest_writer = Arc::clone(&latest);
        let mut latest_rx = self.snapshot_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(snapshot) = latest_rx.recv().await {
                if let Ok(json) = serde_json::to_string(&snapshot) {
                    *latest_writer.write().await = Some(json);
                }
            }
        });

        let command_ids = Arc::new(Mutex::new(HashMap::<String, i64>::new()));
        loop {
            let (stream, peer) = listener.accept().await?;
            let command_tx = self.command_tx.clone();
            let control_tx = self.control_tx.clone();
            let control_token = self.control_token.clone();
            let dashboard_token = Arc::clone(&self.dashboard_token);
            let snapshot_rx = self.snapshot_tx.subscribe();
            let latest = Arc::clone(&latest);
            let command_ids = Arc::clone(&command_ids);
            tokio::spawn(async move {
                let context = ConnectionContext {
                    command_tx,
                    control_tx,
                    control_token,
                    dashboard_token,
                    snapshot_rx,
                    latest,
                    command_ids,
                };
                if let Err(error) = handle_connection(stream, peer, context).await {
                    warn!(%peer, %error, "dashboard connection closed with an error");
                }
            });
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    mut context: ConnectionContext,
) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let local_addr = stream.local_addr()?;
    let request_bytes = read_request(&mut stream).await?;
    let request = String::from_utf8_lossy(&request_bytes);
    let first_line = request.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/");

    if !request_host_matches_listener(&request, local_addr) {
        return write_response(
            &mut stream,
            "421 Misdirected Request",
            "application/json",
            "{\"error\":\"host_rejected\"}\n",
        )
        .await;
    }

    if (method == "GET" || method == "POST") && path == "/_auth/session" {
        return handle_dashboard_session_request(
            &mut stream,
            peer,
            method,
            &request,
            &context.dashboard_token,
        )
        .await;
    }

    if method == "GET" && path == "/command-ws" {
        if !dashboard_session_authorized(peer, &request, &context.dashboard_token) {
            return write_response(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                "{\"error\":\"dashboard_auth_required\"}\n",
            )
            .await;
        }
        if !same_origin_or_non_browser(&request) {
            return write_response(
                &mut stream,
                "403 Forbidden",
                "application/json",
                "{\"error\":\"origin_rejected\"}\n",
            )
            .await;
        }
        return upgrade_command_websocket(
            stream,
            &request,
            context.command_tx,
            context.command_ids,
        )
        .await;
    }

    if method == "POST" && path == "/cmd" {
        if !dashboard_session_authorized(peer, &request, &context.dashboard_token) {
            return write_response(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                "{\"error\":\"dashboard_auth_required\"}\n",
            )
            .await;
        }
        if !same_origin_or_non_browser(&request) {
            return write_response(
                &mut stream,
                "403 Forbidden",
                "application/json",
                "{\"error\":\"origin_rejected\"}\n",
            )
            .await;
        }
        let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        return handle_http_command(&mut stream, body, context.command_tx, context.command_ids)
            .await;
    }

    if method == "POST" && path == "/_control/shutdown" {
        return handle_shutdown_request(
            &mut stream,
            peer,
            &request,
            context.control_token.as_deref(),
            context.control_tx,
        )
        .await;
    }

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                DASHBOARD_HTML,
            )
            .await
        }
        ("GET", "/healthz") => {
            write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}\n").await
        }
        ("GET", "/events") => {
            info!(%peer, "dashboard event stream connected");
            let headers = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: text/event-stream\r\n",
                "Cache-Control: no-cache\r\n",
                "Connection: keep-alive\r\n",
                "X-Content-Type-Options: nosniff\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).await?;
            if let Some(snapshot) = context.latest.read().await.as_ref() {
                stream
                    .write_all(format!("data: {snapshot}\n\n").as_bytes())
                    .await?;
            }
            loop {
                match context.snapshot_rx.recv().await {
                    Ok(snapshot) => {
                        let json = serde_json::to_string(&snapshot)?;
                        stream
                            .write_all(format!("data: {json}\n\n").as_bytes())
                            .await?;
                        stream.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
        _ => {
            write_response(
                &mut stream,
                "404 Not Found",
                "application/json",
                "{\"error\":\"not_found\"}\n",
            )
            .await
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(4_096);
    loop {
        let mut chunk = [0_u8; 2_048];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.len() > MAX_REQUEST_BYTES {
            anyhow::bail!("request exceeds {MAX_REQUEST_BYTES} bytes");
        }
        let Some(headers_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buffer[..headers_end]);
        let content_length = header_value(&headers, "Content-Length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if content_length > MAX_REQUEST_BYTES {
            anyhow::bail!("request body exceeds {MAX_REQUEST_BYTES} bytes");
        }
        if buffer.len() >= headers_end + 4 + content_length {
            break;
        }
    }
    Ok(buffer)
}

fn header_value<'a>(request: &'a str, wanted: &str) -> Option<&'a str> {
    request.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(wanted).then(|| value.trim())
    })
}

fn same_origin_or_non_browser(request: &str) -> bool {
    let Some(origin) = header_value(request, "Origin") else {
        return true;
    };
    let Some(host) = header_value(request, "Host") else {
        return false;
    };
    let origin = origin.trim_end_matches('/');
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
        || origin.eq_ignore_ascii_case(&format!("https://{host}"))
}

fn request_host_matches_listener(request: &str, local_addr: SocketAddr) -> bool {
    let Some(host) = header_value(request, "Host") else {
        return false;
    };
    if let Ok(address) = host.parse::<SocketAddr>() {
        return address == local_addr;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip == local_addr.ip() && local_addr.port() == 80;
    }
    let localhost_port = host
        .strip_prefix("localhost:")
        .and_then(|port| port.parse::<u16>().ok());
    (host.eq_ignore_ascii_case("localhost")
        && local_addr.ip().is_loopback()
        && local_addr.port() == 80)
        || (localhost_port == Some(local_addr.port()) && local_addr.ip().is_loopback())
}

fn constant_time_token_eq(provided: &str, expected: &str) -> bool {
    let mut difference = provided.len() ^ expected.len();
    for index in 0..provided.len().max(expected.len()) {
        let left = provided.as_bytes().get(index).copied().unwrap_or_default();
        let right = expected.as_bytes().get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn cookie_value<'a>(request: &'a str, wanted: &str) -> Option<&'a str> {
    let cookie_header = header_value(request, "Cookie")?;
    let mut found = None;
    for pair in cookie_header.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        if name != wanted {
            continue;
        }
        if found.is_some() || value.is_empty() {
            return None;
        }
        found = Some(value);
    }
    found
}

fn dashboard_session_authorized(peer: SocketAddr, request: &str, expected_token: &str) -> bool {
    peer.ip().is_loopback()
        && cookie_value(request, DASHBOARD_SESSION_COOKIE)
            .is_some_and(|provided| constant_time_token_eq(provided, expected_token))
}

async fn handle_dashboard_session_request(
    stream: &mut TcpStream,
    peer: SocketAddr,
    method: &str,
    request: &str,
    expected_token: &str,
) -> anyhow::Result<()> {
    if method == "GET" {
        if dashboard_session_authorized(peer, request, expected_token) {
            return write_response(stream, "200 OK", "application/json", "{\"ok\":true}\n").await;
        }
        return write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            "{\"error\":\"dashboard_auth_required\"}\n",
        )
        .await;
    }

    let provided =
        header_value(request, "Authorization").and_then(|value| value.strip_prefix("Bearer "));
    if !peer.ip().is_loopback()
        || !same_origin_or_non_browser(request)
        || !provided.is_some_and(|provided| constant_time_token_eq(provided, expected_token))
    {
        return write_response(
            stream,
            "403 Forbidden",
            "application/json",
            "{\"error\":\"dashboard_bootstrap_rejected\"}\n",
        )
        .await;
    }

    let cookie =
        format!("{DASHBOARD_SESSION_COOKIE}={expected_token}; HttpOnly; SameSite=Strict; Path=/");
    write_response_with_headers(
        stream,
        "204 No Content",
        "application/json",
        "",
        &[("Set-Cookie", &cookie)],
    )
    .await
}

async fn handle_shutdown_request(
    stream: &mut TcpStream,
    peer: SocketAddr,
    request: &str,
    expected_token: Option<&str>,
    control_tx: mpsc::Sender<DashboardControl>,
) -> anyhow::Result<()> {
    let Some(expected_token) = expected_token else {
        return write_response(
            stream,
            "404 Not Found",
            "application/json",
            "{\"error\":\"not_found\"}\n",
        )
        .await;
    };
    let provided =
        header_value(request, "Authorization").and_then(|value| value.strip_prefix("Bearer "));
    if !peer.ip().is_loopback()
        || !provided.is_some_and(|provided| constant_time_token_eq(provided, expected_token))
    {
        return write_response(
            stream,
            "403 Forbidden",
            "application/json",
            "{\"error\":\"forbidden\"}\n",
        )
        .await;
    }
    match control_tx.try_reserve_owned() {
        Ok(permit) => {
            write_response(
                stream,
                "202 Accepted",
                "application/json",
                "{\"ok\":true}\n",
            )
            .await?;
            permit.send(DashboardControl::Shutdown);
            Ok(())
        }
        Err(_) => {
            write_response(
                stream,
                "503 Service Unavailable",
                "application/json",
                "{\"error\":\"control_unavailable\"}\n",
            )
            .await
        }
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> anyhow::Result<()> {
    write_response_with_headers(stream, status, content_type, body, &[]).await
}

async fn write_response_with_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:\r\n",
        status,
        content_type,
        body.len(),
    );
    for (name, value) in extra_headers {
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            anyhow::bail!("response header contains a line break");
        }
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("Connection: close\r\n\r\n");
    response.push_str(body);
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn handle_http_command(
    stream: &mut TcpStream,
    body: &str,
    command_tx: mpsc::Sender<DashboardCommand>,
    command_ids: Arc<Mutex<HashMap<String, i64>>>,
) -> anyhow::Result<()> {
    let (status, ack) = queue_command(body, command_tx, command_ids).await;
    let body = format!("{}\n", serde_json::to_string(&ack)?);
    write_response(stream, status, "application/json", &body).await
}

async fn upgrade_command_websocket(
    mut stream: TcpStream,
    request: &str,
    command_tx: mpsc::Sender<DashboardCommand>,
    command_ids: Arc<Mutex<HashMap<String, i64>>>,
) -> anyhow::Result<()> {
    let key = header_value(request, "Sec-WebSocket-Key")
        .ok_or_else(|| anyhow::anyhow!("missing Sec-WebSocket-Key"))?;
    if !header_value(request, "Upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    {
        anyhow::bail!("websocket upgrade header is missing");
    }
    let accept_key = derive_accept_key(key.as_bytes());
    stream
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept_key}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    stream.flush().await?;
    let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    while let Some(message) = websocket.next().await {
        match message? {
            Message::Text(text) => {
                let (_, ack) =
                    queue_command(text.as_str(), command_tx.clone(), Arc::clone(&command_ids))
                        .await;
                websocket
                    .send(Message::Text(serde_json::to_string(&ack)?.into()))
                    .await?;
            }
            Message::Ping(payload) => websocket.send(Message::Pong(payload)).await?,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(())
}

async fn queue_command(
    body: &str,
    command_tx: mpsc::Sender<DashboardCommand>,
    command_ids: Arc<Mutex<HashMap<String, i64>>>,
) -> (&'static str, serde_json::Value) {
    let command = match parse_dashboard_command(body) {
        Ok(command) => command,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"kind":"command_ack","ok":false,"error":error.to_string()}),
            );
        }
    };
    let request_id = command.request_id.clone();
    if remember_command_id(&command_ids, &request_id).await {
        return (
            "200 OK",
            serde_json::json!({"kind":"command_ack","request_id":request_id,"ok":true,"duplicate":true}),
        );
    }
    let received_at_ms = command.dashboard_received_at_ms;
    match command_tx.try_send(command) {
        Ok(()) => (
            "200 OK",
            serde_json::json!({"kind":"command_ack","request_id":request_id,"ok":true,"duplicate":false,"dashboard_received_at_ms":received_at_ms}),
        ),
        Err(error) => {
            command_ids.lock().await.remove(&request_id);
            (
                "503 Service Unavailable",
                serde_json::json!({"kind":"command_ack","request_id":request_id,"ok":false,"error":format!("command queue unavailable: {error}")}),
            )
        }
    }
}

async fn remember_command_id(command_ids: &Mutex<HashMap<String, i64>>, request_id: &str) -> bool {
    const RETENTION_MS: i64 = 5 * 60 * 1_000;
    const MAX_IDS: usize = 2_048;
    let now = now_ms();
    let mut ids = command_ids.lock().await;
    ids.retain(|_, seen_at| now.saturating_sub(*seen_at) <= RETENTION_MS);
    if ids.contains_key(request_id) {
        return true;
    }
    if ids.len() >= MAX_IDS
        && let Some(oldest) = ids
            .iter()
            .min_by_key(|(_, timestamp)| **timestamp)
            .map(|(id, _)| id.clone())
    {
        ids.remove(&oldest);
    }
    ids.insert(request_id.to_string(), now);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_atomic_lightweight_commands() {
        let command = parse_dashboard_command(
            r#"{"request_id":"request-1","command":{"type":"submit_order","payload":{"side":"buy_up","nominal_usd":1.0,"mechanism":"taker","expected_session_slug":"btc-updown-5m-1"}}}"#,
        )
        .expect("parse command");
        assert_eq!(command.request_id, "request-1");
        assert!(matches!(
            command.command,
            DashboardCmd::SubmitOrder {
                side: TradeSide::BuyUp,
                ..
            }
        ));
    }

    #[test]
    fn embedded_dashboard_contains_consumer_controls() {
        for required in [
            "id=\"price-chart\"",
            "id=\"current-pnl\"",
            "id=\"daily-pnl\"",
            "id=\"trades\"",
            "id=\"claims\"",
            "data-side=\"buy_up\"",
            "/_auth/session",
        ] {
            assert!(
                DASHBOARD_HTML.contains(required),
                "dashboard is missing {required}"
            );
        }
    }

    #[test]
    fn browser_command_origin_must_match_host() {
        assert!(same_origin_or_non_browser(
            "POST /cmd HTTP/1.1\r\nHost: 127.0.0.1:9878\r\nOrigin: http://127.0.0.1:9878\r\n\r\n"
        ));
        assert!(!same_origin_or_non_browser(
            "POST /cmd HTTP/1.1\r\nHost: 127.0.0.1:9878\r\nOrigin: https://attacker.example\r\n\r\n"
        ));
    }

    #[test]
    fn request_host_must_resolve_to_the_listener_address() {
        let local: SocketAddr = "127.0.0.1:9878".parse().expect("address");
        assert!(request_host_matches_listener(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:9878\r\n\r\n",
            local
        ));
        assert!(request_host_matches_listener(
            "GET / HTTP/1.1\r\nHost: localhost:9878\r\n\r\n",
            local
        ));
        assert!(!request_host_matches_listener(
            "GET / HTTP/1.1\r\nHost: attacker.example:9878\r\n\r\n",
            local
        ));
    }

    #[test]
    fn control_token_comparison_handles_length_and_content() {
        assert!(constant_time_token_eq("token", "token"));
        assert!(!constant_time_token_eq("token", "tokens"));
        assert!(!constant_time_token_eq("t0ken", "token"));
    }

    #[test]
    fn dashboard_session_requires_one_exact_cookie() {
        let peer: SocketAddr = "127.0.0.1:50000".parse().expect("peer");
        assert!(dashboard_session_authorized(
            peer,
            "GET /command-ws HTTP/1.1\r\nHost: 127.0.0.1:9878\r\nCookie: theme=dark; polytread_dashboard_session=token\r\n\r\n",
            "token"
        ));
        assert!(!dashboard_session_authorized(
            peer,
            "GET /command-ws HTTP/1.1\r\nHost: 127.0.0.1:9878\r\n\r\n",
            "token"
        ));
        assert!(!dashboard_session_authorized(
            peer,
            "GET /command-ws HTTP/1.1\r\nHost: 127.0.0.1:9878\r\nCookie: polytread_dashboard_session=token; polytread_dashboard_session=other\r\n\r\n",
            "token"
        ));
    }

    async fn send_raw_request(address: SocketAddr, request: String) -> String {
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("response");
        String::from_utf8(response).expect("UTF-8 response")
    }

    #[tokio::test]
    async fn command_endpoint_requires_dashboard_session() {
        let (snapshot_tx, _) = broadcast::channel(4);
        let (server, mut command_rx, _control_rx) =
            DashboardServer::new(snapshot_tx, None, "dashboard-secret".to_string());
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move { server.run("127.0.0.1:0", ready_tx).await });
        let address = ready_rx.await.expect("listener ready");
        let body = r#"{"request_id":"unauthenticated","command":{"type":"set_enabled","payload":{"enabled":false}}}"#;
        let response = send_raw_request(
            address,
            format!(
                "POST /cmd HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(response.contains("401 Unauthorized"));
        assert!(command_rx.try_recv().is_err());
        task.abort();
    }

    #[tokio::test]
    async fn bootstrap_cookie_authorizes_a_safe_command() {
        let (snapshot_tx, _) = broadcast::channel(4);
        let (server, mut command_rx, _control_rx) =
            DashboardServer::new(snapshot_tx, None, "dashboard-secret".to_string());
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move { server.run("127.0.0.1:0", ready_tx).await });
        let address = ready_rx.await.expect("listener ready");

        let bootstrap = send_raw_request(
            address,
            format!(
                "POST /_auth/session HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nAuthorization: Bearer dashboard-secret\r\nContent-Length: 0\r\n\r\n"
            ),
        )
        .await;
        assert!(bootstrap.contains("204 No Content"));
        let cookie = bootstrap
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .and_then(|value| value.split(';').next())
            .expect("session cookie");

        let body = r#"{"request_id":"safe-command","command":{"type":"set_enabled","payload":{"enabled":false}}}"#;
        let command_response = send_raw_request(
            address,
            format!(
                "POST /cmd HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(command_response.contains("200 OK"));
        let command = command_rx.recv().await.expect("queued command");
        assert!(matches!(
            command.command,
            DashboardCmd::SetEnabled { enabled: false }
        ));
        task.abort();
    }

    #[tokio::test]
    async fn authenticated_loopback_shutdown_enters_control_queue() {
        let (snapshot_tx, _) = broadcast::channel(4);
        let (server, _command_rx, mut control_rx) = DashboardServer::new(
            snapshot_tx,
            Some("local-secret".to_string()),
            "dashboard-secret".to_string(),
        );
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move { server.run("127.0.0.1:0", ready_tx).await });
        let address = ready_rx.await.expect("listener ready");

        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(
                format!(
                    "POST /_control/shutdown HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer local-secret\r\nContent-Length: 0\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("response");
        assert!(String::from_utf8_lossy(&response).contains("202 Accepted"));
        assert_eq!(control_rx.recv().await, Some(DashboardControl::Shutdown));
        task.abort();
    }
}
