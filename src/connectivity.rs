use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::net::lookup_host;
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite;

use crate::config::{CLOB_API_URL, GAMMA_API_URL, MARKET_WS_URL};
use crate::state::{AppEvent, now_ms};

const CLOB_HOST: &str = "clob.polymarket.com";
const CLOUDFLARE_DOH_HOST: &str = "cloudflare-dns.com";
const CLOUDFLARE_DOH_URL: &str = "https://cloudflare-dns.com/dns-query";
const CONNECTIVITY_INTERVAL: Duration = Duration::from_secs(30);
const DNS_TIMEOUT: Duration = Duration::from_secs(6);
const WEBSOCKET_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivityKind {
    Checking,
    Available,
    Degraded,
    DnsFiltering,
    EndpointRestricted,
    Unreachable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectivityStatus {
    pub kind: ConnectivityKind,
    pub headline: String,
    pub detail: String,
    pub checked_at_ms: i64,
    pub clob_rest_ok: bool,
    pub market_rest_ok: bool,
    pub market_websocket_ok: bool,
    pub setup_ready: bool,
}

impl Default for ConnectivityStatus {
    fn default() -> Self {
        Self {
            kind: ConnectivityKind::Checking,
            headline: "Checking Polymarket connectivity".to_string(),
            detail: "PolyTread is testing real REST and WebSocket endpoints.".to_string(),
            checked_at_ms: now_ms(),
            clob_rest_ok: false,
            market_rest_ok: false,
            market_websocket_ok: false,
            setup_ready: false,
        }
    }
}

impl ConnectivityStatus {
    pub fn needs_dns_remediation(&self) -> bool {
        self.kind == ConnectivityKind::DnsFiltering
    }

    pub fn is_usable_for_setup(&self) -> bool {
        self.setup_ready
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    Healthy,
    Denied,
    Responded,
    Failed,
}

#[derive(Debug, Clone)]
struct EndpointProbe {
    state: ProbeState,
    detail: String,
}

impl EndpointProbe {
    fn healthy(&self) -> bool {
        self.state == ProbeState::Healthy
    }

    fn denied(&self) -> bool {
        self.state == ProbeState::Denied
    }

    fn received_response(&self) -> bool {
        self.state != ProbeState::Failed
    }
}

#[derive(Debug, Default)]
struct DnsComparison {
    system: BTreeSet<IpAddr>,
    encrypted: BTreeSet<IpAddr>,
    encrypted_destination_works: bool,
}

impl DnsComparison {
    fn confirms_dns_filtering(&self) -> bool {
        let resolver_disagrees = (!self.system.is_empty()
            && !self.encrypted.is_empty()
            && self.system.is_disjoint(&self.encrypted))
            || (self.system.is_empty() && !self.encrypted.is_empty());
        resolver_disagrees && self.encrypted_destination_works
    }
}

#[derive(Debug, Deserialize)]
struct DohResponse {
    #[serde(rename = "Status")]
    status: i32,
    #[serde(rename = "Answer", default)]
    answers: Vec<DohAnswer>,
}

#[derive(Debug, Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    record_type: u16,
    data: String,
}

pub async fn probe(client: &Client) -> ConnectivityStatus {
    let (clob, market_rest, market_websocket) = tokio::join!(
        probe_clob_time(client),
        probe_market_rest(client),
        probe_market_websocket(),
    );

    let dns = if clob.healthy() && market_rest.healthy() {
        DnsComparison::default()
    } else {
        compare_clob_dns().await
    };

    classify(clob, market_rest, market_websocket, dns)
}

pub async fn run_monitor(
    client: Client,
    event_tx: mpsc::Sender<AppEvent>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(CONNECTIVITY_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = interval.tick() => {
                if event_tx.send(AppEvent::Connectivity(probe(&client).await)).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn classify(
    clob: EndpointProbe,
    market_rest: EndpointProbe,
    market_websocket: EndpointProbe,
    dns: DnsComparison,
) -> ConnectivityStatus {
    let clob_ok = clob.healthy();
    let market_rest_ok = market_rest.healthy();
    let websocket_ok = market_websocket.healthy();
    let setup_ready = clob_ok && market_rest_ok;
    let required_denied = clob.denied() || market_rest.denied();
    let any_denied = required_denied || market_websocket.denied();
    let any_healthy = clob_ok || market_rest_ok || websocket_ok;
    let any_response = clob.received_response()
        || market_rest.received_response()
        || market_websocket.received_response();

    let (kind, headline, detail) = if setup_ready && websocket_ok {
        (
            ConnectivityKind::Available,
            "Polymarket endpoints are reachable",
            "Real CLOB REST, market-data REST, and market WebSocket checks passed.",
        )
    } else if required_denied || (!any_healthy && any_denied) {
        (
            ConnectivityKind::EndpointRestricted,
            "A real Polymarket endpoint rejected this connection",
            "The service responded with an access-denied status. Changing DNS would not fix this response.",
        )
    } else if any_healthy {
        (
            ConnectivityKind::Degraded,
            "Polymarket connectivity is degraded",
            "At least one real endpoint works, but the complete REST and WebSocket path is not ready.",
        )
    } else if !any_response && dns.confirms_dns_filtering() {
        (
            ConnectivityKind::DnsFiltering,
            "DNS or ISP filtering detected",
            "The system resolver differs from encrypted DNS, and a real CLOB request succeeds through the encrypted-DNS destination. Browser Secure DNS may still work while terminal applications fail.",
        )
    } else {
        (
            ConnectivityKind::Unreachable,
            "Polymarket endpoints are currently unreachable",
            "DNS filtering was not confirmed. The cause may be a service outage, firewall, or wider network failure.",
        )
    };

    let failed_detail = [clob, market_rest, market_websocket]
        .into_iter()
        .filter(|probe| !probe.healthy())
        .map(|probe| probe.detail)
        .collect::<Vec<_>>()
        .join(" ");
    let detail = if failed_detail.is_empty() {
        detail.to_string()
    } else {
        format!("{detail} {failed_detail}")
    };

    ConnectivityStatus {
        kind,
        headline: headline.to_string(),
        detail,
        checked_at_ms: now_ms(),
        clob_rest_ok: clob_ok,
        market_rest_ok,
        market_websocket_ok: websocket_ok,
        setup_ready,
    }
}

async fn probe_clob_time(client: &Client) -> EndpointProbe {
    let response = match client.get(format!("{CLOB_API_URL}/time")).send().await {
        Ok(response) => response,
        Err(error) => {
            return EndpointProbe {
                state: ProbeState::Failed,
                detail: format!("CLOB REST failed: {}.", request_error_label(&error)),
            };
        }
    };
    let status = response.status();
    if is_denied(status) {
        return EndpointProbe {
            state: ProbeState::Denied,
            detail: format!("CLOB REST returned HTTP {status}."),
        };
    }
    if !status.is_success() {
        return EndpointProbe {
            state: ProbeState::Responded,
            detail: format!("CLOB REST returned HTTP {status}."),
        };
    }
    match response.text().await {
        Ok(body) if body.trim().trim_matches('"').parse::<i64>().is_ok() => EndpointProbe {
            state: ProbeState::Healthy,
            detail: String::new(),
        },
        Ok(_) => EndpointProbe {
            state: ProbeState::Responded,
            detail: "CLOB REST returned an invalid time response.".to_string(),
        },
        Err(error) => EndpointProbe {
            state: ProbeState::Responded,
            detail: format!(
                "CLOB REST response failed: {}.",
                request_error_label(&error)
            ),
        },
    }
}

async fn probe_market_rest(client: &Client) -> EndpointProbe {
    let response = match client
        .get(format!("{GAMMA_API_URL}/events"))
        .query(&[("active", "true"), ("closed", "false"), ("limit", "1")])
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return EndpointProbe {
                state: ProbeState::Failed,
                detail: format!("Market REST failed: {}.", request_error_label(&error)),
            };
        }
    };
    let status = response.status();
    if is_denied(status) {
        return EndpointProbe {
            state: ProbeState::Denied,
            detail: format!("Market REST returned HTTP {status}."),
        };
    }
    if !status.is_success() {
        return EndpointProbe {
            state: ProbeState::Responded,
            detail: format!("Market REST returned HTTP {status}."),
        };
    }
    match response.json::<serde_json::Value>().await {
        Ok(value) if value.is_array() => EndpointProbe {
            state: ProbeState::Healthy,
            detail: String::new(),
        },
        Ok(_) => EndpointProbe {
            state: ProbeState::Responded,
            detail: "Market REST returned an unexpected response.".to_string(),
        },
        Err(error) => EndpointProbe {
            state: ProbeState::Responded,
            detail: format!(
                "Market REST response failed: {}.",
                request_error_label(&error)
            ),
        },
    }
}

async fn probe_market_websocket() -> EndpointProbe {
    let result = tokio::time::timeout(
        WEBSOCKET_TIMEOUT,
        tokio_tungstenite::connect_async(MARKET_WS_URL),
    )
    .await;
    match result {
        Ok(Ok((mut socket, _))) => {
            let _ = socket.close(None).await;
            EndpointProbe {
                state: ProbeState::Healthy,
                detail: String::new(),
            }
        }
        Ok(Err(tungstenite::Error::Http(response))) if is_denied(response.status()) => {
            EndpointProbe {
                state: ProbeState::Denied,
                detail: format!("Market WebSocket returned HTTP {}.", response.status()),
            }
        }
        Ok(Err(tungstenite::Error::Http(response))) => EndpointProbe {
            state: ProbeState::Responded,
            detail: format!("Market WebSocket returned HTTP {}.", response.status()),
        },
        Ok(Err(error)) => EndpointProbe {
            state: ProbeState::Failed,
            detail: format!(
                "Market WebSocket failed: {}.",
                websocket_error_label(&error)
            ),
        },
        Err(_) => EndpointProbe {
            state: ProbeState::Failed,
            detail: "Market WebSocket timed out.".to_string(),
        },
    }
}

async fn compare_clob_dns() -> DnsComparison {
    let (system, encrypted) = tokio::join!(resolve_system(), resolve_encrypted());
    let resolver_disagrees =
        (!system.is_empty() && !encrypted.is_empty() && system.is_disjoint(&encrypted))
            || (system.is_empty() && !encrypted.is_empty());
    let encrypted_destination_works = if resolver_disagrees {
        verify_encrypted_clob_destination(&encrypted).await
    } else {
        false
    };
    DnsComparison {
        system,
        encrypted,
        encrypted_destination_works,
    }
}

async fn verify_encrypted_clob_destination(addresses: &BTreeSet<IpAddr>) -> bool {
    for address in addresses.iter().copied().take(4) {
        let client = match Client::builder()
            .timeout(DNS_TIMEOUT)
            .resolve(CLOB_HOST, SocketAddr::new(address, 443))
            .build()
        {
            Ok(client) => client,
            Err(_) => continue,
        };
        if probe_clob_time(&client).await.healthy() {
            return true;
        }
    }
    false
}

async fn resolve_system() -> BTreeSet<IpAddr> {
    match tokio::time::timeout(DNS_TIMEOUT, lookup_host((CLOB_HOST, 443))).await {
        Ok(Ok(addresses)) => addresses
            .map(|address| address.ip())
            .filter(IpAddr::is_ipv4)
            .collect(),
        _ => BTreeSet::new(),
    }
}

async fn resolve_encrypted() -> BTreeSet<IpAddr> {
    let bootstrap = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443);
    let client = match Client::builder()
        .timeout(DNS_TIMEOUT)
        .resolve(CLOUDFLARE_DOH_HOST, bootstrap)
        .build()
    {
        Ok(client) => client,
        Err(_) => return BTreeSet::new(),
    };
    let response = match client
        .get(CLOUDFLARE_DOH_URL)
        .header("accept", "application/dns-json")
        .query(&[("name", CLOB_HOST), ("type", "A")])
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        _ => return BTreeSet::new(),
    };
    let body = match response.json::<DohResponse>().await {
        Ok(body) if body.status == 0 => body,
        _ => return BTreeSet::new(),
    };
    body.answers
        .into_iter()
        .filter(|answer| answer.record_type == 1)
        .filter_map(|answer| answer.data.parse::<IpAddr>().ok())
        .collect()
}

fn is_denied(status: StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403 | 451)
}

fn request_error_label(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "connection timed out"
    } else if error.is_connect() {
        "connection could not be established"
    } else if error.is_decode() {
        "response could not be decoded"
    } else {
        "request failed"
    }
}

fn websocket_error_label(error: &tungstenite::Error) -> &'static str {
    match error {
        tungstenite::Error::Io(io_error) if io_error.kind() == std::io::ErrorKind::TimedOut => {
            "connection timed out"
        }
        tungstenite::Error::Io(_) => "connection could not be established",
        tungstenite::Error::Tls(_) => "TLS handshake failed",
        _ => "handshake failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(state: ProbeState, detail: &str) -> EndpointProbe {
        EndpointProbe {
            state,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn classifies_real_endpoint_success_as_available() {
        let status = classify(
            endpoint(ProbeState::Healthy, ""),
            endpoint(ProbeState::Healthy, ""),
            endpoint(ProbeState::Healthy, ""),
            DnsComparison::default(),
        );
        assert_eq!(status.kind, ConnectivityKind::Available);
        assert!(status.setup_ready);
    }

    #[test]
    fn classifies_disjoint_system_and_encrypted_dns_as_filtering() {
        let status = classify(
            endpoint(ProbeState::Failed, "CLOB failed."),
            endpoint(ProbeState::Failed, "Market REST failed."),
            endpoint(ProbeState::Failed, "WebSocket failed."),
            DnsComparison {
                system: ["192.0.2.10".parse().expect("IP")].into_iter().collect(),
                encrypted: ["203.0.113.20".parse().expect("IP")].into_iter().collect(),
                encrypted_destination_works: true,
            },
        );
        assert_eq!(status.kind, ConnectivityKind::DnsFiltering);
        assert!(!status.setup_ready);
    }

    #[test]
    fn real_access_denial_is_not_mislabeled_as_dns_filtering() {
        let status = classify(
            endpoint(ProbeState::Denied, "CLOB returned HTTP 403."),
            endpoint(ProbeState::Failed, "Market REST failed."),
            endpoint(ProbeState::Failed, "WebSocket failed."),
            DnsComparison {
                system: ["192.0.2.10".parse().expect("IP")].into_iter().collect(),
                encrypted: ["203.0.113.20".parse().expect("IP")].into_iter().collect(),
                encrypted_destination_works: true,
            },
        );
        assert_eq!(status.kind, ConnectivityKind::EndpointRestricted);
    }

    #[test]
    fn real_service_response_prevents_false_dns_filtering_label() {
        let status = classify(
            endpoint(ProbeState::Responded, "CLOB returned HTTP 503."),
            endpoint(ProbeState::Failed, "Market REST failed."),
            endpoint(ProbeState::Failed, "WebSocket failed."),
            DnsComparison {
                system: ["192.0.2.10".parse().expect("IP")].into_iter().collect(),
                encrypted: ["203.0.113.20".parse().expect("IP")].into_iter().collect(),
                encrypted_destination_works: true,
            },
        );
        assert_eq!(status.kind, ConnectivityKind::Unreachable);
    }

    #[test]
    fn dns_disagreement_without_a_working_real_destination_is_unreachable() {
        let status = classify(
            endpoint(ProbeState::Failed, "CLOB failed."),
            endpoint(ProbeState::Failed, "Market REST failed."),
            endpoint(ProbeState::Failed, "WebSocket failed."),
            DnsComparison {
                system: ["192.0.2.10".parse().expect("IP")].into_iter().collect(),
                encrypted: ["203.0.113.20".parse().expect("IP")].into_iter().collect(),
                encrypted_destination_works: false,
            },
        );
        assert_eq!(status.kind, ConnectivityKind::Unreachable);
    }
}
