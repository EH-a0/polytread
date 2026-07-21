use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

use crate::state::{DiscoveryUpdate, SessionDescriptor};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchEvent {
    slug: String,
    title: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    start_time: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    #[serde(default)]
    markets: Vec<SearchMarket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchMarket {
    slug: Option<String>,
    question: Option<String>,
    event_start_time: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    outcomes: Option<String>,
    clob_token_ids: Option<String>,
    order_min_size: Option<f64>,
    order_price_min_tick_size: Option<f64>,
}

pub async fn fetch_sessions_at(
    client: &Client,
    query: &str,
    limit: u32,
    reference_ms: i64,
) -> Result<DiscoveryUpdate> {
    let mut sessions = Vec::new();
    let now_s = reference_ms / 1_000;
    let current_start_s = (now_s / 300) * 300;
    let mut candidate_ts = vec![
        current_start_s - 300,
        current_start_s,
        current_start_s + 300,
    ];
    candidate_ts.sort();
    candidate_ts.dedup();
    let _ = query;
    for timestamp in candidate_ts.into_iter().take(limit as usize) {
        let slug = format!("btc-updown-5m-{timestamp}");
        if let Some(session) = fetch_session_by_slug(client, &slug).await? {
            sessions.push(session);
        }
    }

    sessions.sort_by_key(|session| (session.start_ms, session.end_ms));
    sessions.dedup_by(|left, right| left.slug == right.slug);
    Ok(DiscoveryUpdate {
        fetched_at_ms: reference_ms,
        sessions,
    })
}

async fn fetch_session_by_slug(client: &Client, slug: &str) -> Result<Option<SessionDescriptor>> {
    let response = client
        .get(format!(
            "https://gamma-api.polymarket.com/events/slug/{slug}"
        ))
        .send()
        .await
        .with_context(|| format!("failed fetching event for slug {slug}"))?
        .error_for_status()
        .with_context(|| format!("Gamma slug lookup failed for {slug}"))?;
    let event: SearchEvent = response
        .json()
        .await
        .with_context(|| format!("failed decoding Gamma slug payload for {slug}"))?;
    if !is_btc_five_minute_event(&event) {
        return Ok(None);
    }
    let mut session = event_into_session(event)?;
    session.price_to_beat =
        fetch_target_price(client, &session.slug, session.start_ms, session.end_ms)
            .await
            .ok()
            .flatten();
    Ok(Some(session))
}

fn is_btc_five_minute_event(event: &SearchEvent) -> bool {
    let slug = event.slug.to_ascii_lowercase();
    let title = event.title.to_ascii_lowercase();
    let looks_like_slug = slug.starts_with("btc-updown-5m-");
    if looks_like_slug {
        return true;
    }
    let looks_like_title = title.contains("btc up or down") || title.contains("bitcoin up or down");
    let mentions_five_minutes = title.contains("5 minute") || title.contains("5 minutes");
    let event_window_is_short = match (&event.start_date, &event.end_date) {
        (Some(start), Some(end)) => parse_ms(start)
            .ok()
            .zip(parse_ms(end).ok())
            .map(|(start_ms, end_ms)| (end_ms - start_ms).abs() <= 10 * 60 * 1000)
            .unwrap_or(false),
        _ => false,
    };
    (looks_like_slug || looks_like_title) && (mentions_five_minutes || event_window_is_short)
}

fn event_into_session(event: SearchEvent) -> Result<SessionDescriptor> {
    let event_slug = event.slug.clone();
    let event_title = event.title.clone();
    let event_start_time = event.start_time.clone();
    let event_start = event.start_date.clone();
    let event_end = event.end_date.clone();
    let event_active = event.active;
    let event_closed = event.closed;
    let market = event
        .markets
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("event missing market"))?;
    let title = market
        .question
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(event_title);
    let slug = market.slug.unwrap_or(event_slug);
    let start_ms = market
        .event_start_time
        .as_deref()
        .or(event_start_time.as_deref())
        .map(parse_ms)
        .transpose()?
        .or_else(|| parse_start_ms_from_slug(&slug))
        .or_else(|| {
            market
                .start_date
                .as_deref()
                .and_then(|value| parse_ms(value).ok())
        })
        .or_else(|| {
            event_start
                .as_deref()
                .and_then(|value| parse_ms(value).ok())
        })
        .ok_or_else(|| anyhow!("session missing start time"))?;
    let end_ms = market
        .end_date
        .as_deref()
        .or(event_end.as_deref())
        .map(parse_ms)
        .transpose()?
        .unwrap_or(start_ms + 300_000);
    let outcomes = parse_json_string_array(market.outcomes.as_deref())?;
    let token_ids = parse_json_string_array(market.clob_token_ids.as_deref())?;
    if token_ids.len() < 2 {
        return Err(anyhow!("session missing token ids"));
    }

    let mut up_token_id = token_ids.first().cloned().unwrap_or_default();
    let mut down_token_id = token_ids.get(1).cloned().unwrap_or_default();
    for (outcome, token) in outcomes.iter().zip(token_ids.iter()) {
        match normalize_updown_outcome(outcome) {
            Some(true) => up_token_id = token.clone(),
            Some(false) => down_token_id = token.clone(),
            None => {}
        }
    }

    Ok(SessionDescriptor {
        slug,
        title,
        start_ms,
        end_ms,
        price_to_beat: None,
        up_token_id,
        down_token_id,
        active: event_active,
        closed: event_closed,
        minimum_order_size: market
            .order_min_size
            .filter(|value| value.is_finite() && *value > 0.0),
        tick_size: market
            .order_price_min_tick_size
            .filter(|value| value.is_finite() && *value > 0.0),
    })
}

fn parse_json_string_array(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str(raw).context("failed parsing JSON string array")
}

fn normalize_updown_outcome(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "up" | "yes" => Some(true),
        "down" | "no" => Some(false),
        _ => None,
    }
}

fn parse_ms(raw: &str) -> Result<i64> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC3339 timestamp: {raw}"))?;
    Ok(parsed.timestamp_millis())
}

fn parse_start_ms_from_slug(slug: &str) -> Option<i64> {
    slug.strip_prefix("btc-updown-5m-")
        .and_then(|value| value.parse::<i64>().ok())
        .map(|value| value * 1_000)
}

async fn fetch_target_price(
    client: &Client,
    slug: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Option<f64>> {
    let body = client
        .get(format!("https://polymarket.com/event/{slug}"))
        .send()
        .await
        .with_context(|| format!("failed fetching event page for {slug}"))?
        .error_for_status()
        .with_context(|| format!("event page returned error status for {slug}"))?
        .text()
        .await
        .with_context(|| format!("failed reading event page for {slug}"))?;
    Ok(extract_target_price(&body, slug, start_ms, end_ms))
}

fn extract_target_price(body: &str, slug: &str, start_ms: i64, end_ms: i64) -> Option<f64> {
    let window_open_price = extract_window_open_price(body, start_ms, end_ms);
    let event_price_to_beat = extract_event_query_price_to_beat(body, slug)
        .or_else(|| extract_slug_price_to_beat(body, slug));
    window_open_price.or(event_price_to_beat)
}

fn extract_window_open_price(body: &str, start_ms: i64, end_ms: i64) -> Option<f64> {
    let start = DateTime::from_timestamp_millis(start_ms)?
        .with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let end = DateTime::from_timestamp_millis(end_ms)?
        .with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let query_key = format!(
        "\"queryKey\":[\"crypto-prices\",\"price\",\"BTC\",\"{start}\",\"fiveminute\",\"{end}\"]"
    );
    extract_number_before_query_key(body, &query_key, "\"openPrice\":", 4_096)
}

fn extract_event_query_price_to_beat(body: &str, slug: &str) -> Option<f64> {
    let query_key = format!("\"queryKey\":[\"/api/event/slug\",\"{slug}\"]");
    extract_number_before_query_key(body, &query_key, "\"priceToBeat\":", 8_192)
}

fn extract_number_before_query_key(
    body: &str,
    query_key: &str,
    marker: &str,
    search_window: usize,
) -> Option<f64> {
    let query_index = body.find(query_key)?;
    let search_start = query_index.saturating_sub(search_window);
    let prefix = &body[search_start..query_index];
    let marker_index = prefix.rfind(marker)?;
    parse_number_after_marker(&prefix[marker_index..], marker)
}

fn extract_slug_price_to_beat(body: &str, slug: &str) -> Option<f64> {
    let start = body
        .find(&format!("\"ticker\":\"{slug}\""))
        .or_else(|| body.find(&format!("\"slug\":\"{slug}\"")))?;
    let end = (start + 20_000).min(body.len());
    let window = &body[start..end];
    parse_number_after_marker(window, "\"priceToBeat\":")
}

fn parse_number_after_marker(body: &str, marker: &str) -> Option<f64> {
    let start = body.find(marker)? + marker.len();
    let value: String = body[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.' || *ch == '-')
        .collect();
    if value.is_empty() {
        None
    } else {
        value.parse::<f64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SearchEvent, SearchMarket, event_into_session, extract_target_price,
        is_btc_five_minute_event,
    };

    #[test]
    fn event_filter_accepts_expected_slug() {
        let event = SearchEvent {
            slug: "btc-updown-5m-1773378300".to_string(),
            title: "BTC Up Or Down - 5 Minutes".to_string(),
            active: true,
            closed: false,
            start_time: None,
            start_date: Some("2026-03-19T00:00:00Z".to_string()),
            end_date: Some("2026-03-19T00:05:00Z".to_string()),
            markets: Vec::new(),
        };
        assert!(is_btc_five_minute_event(&event));
    }

    #[test]
    fn event_conversion_maps_up_and_down() {
        let event = SearchEvent {
            slug: "btc-updown-5m-1773378300".to_string(),
            title: "BTC Up Or Down - 5 Minutes".to_string(),
            active: true,
            closed: false,
            start_time: Some("2026-03-19T00:00:00Z".to_string()),
            start_date: Some("2026-03-19T00:00:00Z".to_string()),
            end_date: Some("2026-03-19T00:05:00Z".to_string()),
            markets: vec![SearchMarket {
                slug: Some("btc-updown-5m-1773378300".to_string()),
                question: Some("BTC Up Or Down - 5 Minutes".to_string()),
                event_start_time: Some("2026-03-19T00:00:00Z".to_string()),
                start_date: None,
                end_date: None,
                outcomes: Some("[\"Up\",\"Down\"]".to_string()),
                clob_token_ids: Some("[\"up-id\",\"down-id\"]".to_string()),
                order_min_size: Some(5.0),
                order_price_min_tick_size: Some(0.01),
            }],
        };
        let session = event_into_session(event).expect("session");
        assert_eq!(session.up_token_id, "up-id");
        assert_eq!(session.down_token_id, "down-id");
        assert_eq!(session.price_to_beat, None);
        assert_eq!(session.start_ms, 1_773_878_400_000);
        assert_eq!(session.end_ms, 1_773_878_700_000);
        assert_eq!(session.minimum_order_size, Some(5.0));
        assert_eq!(session.tick_size, Some(0.01));
    }

    #[test]
    fn event_conversion_maps_yes_no_even_when_order_is_no_yes() {
        let event = SearchEvent {
            slug: "btc-updown-5m-1773378600".to_string(),
            title: "BTC Up Or Down - 5 Minutes".to_string(),
            active: true,
            closed: false,
            start_time: Some("2026-03-19T00:05:00Z".to_string()),
            start_date: Some("2026-03-19T00:05:00Z".to_string()),
            end_date: Some("2026-03-19T00:10:00Z".to_string()),
            markets: vec![SearchMarket {
                slug: Some("btc-updown-5m-1773378600".to_string()),
                question: Some("BTC Up Or Down - 5 Minutes".to_string()),
                event_start_time: Some("2026-03-19T00:05:00Z".to_string()),
                start_date: None,
                end_date: None,
                outcomes: Some("[\"No\",\"Yes\"]".to_string()),
                clob_token_ids: Some("[\"no-id\",\"yes-id\"]".to_string()),
                order_min_size: Some(5.0),
                order_price_min_tick_size: Some(0.01),
            }],
        };
        let session = event_into_session(event).expect("session");
        assert_eq!(session.up_token_id, "yes-id");
        assert_eq!(session.down_token_id, "no-id");
    }

    #[test]
    fn target_price_uses_matching_crypto_price_window() {
        let body = r#"
            {"state":{"data":{"id":"old","eventMetadata":{"priceToBeat":69999.30}}},
             "state":{"data":{"openPrice":71109.60719602449,"closePrice":null},
             "queryKey":["crypto-prices","price","BTC","2026-03-20T08:45:00Z","fiveminute","2026-03-20T08:50:00Z"]}}
        "#;
        let extracted = extract_target_price(
            body,
            "btc-updown-5m-1773996300",
            1_773_996_300_000,
            1_773_996_600_000,
        );
        assert_eq!(extracted, Some(71_109.60719602449));
    }

    #[test]
    fn target_price_uses_exact_event_query_price_to_beat_before_generic_slug_scan() {
        let body = r#"
            {"ticker":"btc-updown-5m-1773996300","eventMetadata":{"priceToBeat":69999.30}}
            {"state":{"data":{"id":"exact","eventMetadata":{"priceToBeat":71109.60}}},
             "queryKey":["/api/event/slug","btc-updown-5m-1773996300"]}
        "#;
        let extracted = extract_target_price(
            body,
            "btc-updown-5m-1773996300",
            1_773_996_300_000,
            1_773_996_600_000,
        );
        assert_eq!(extracted, Some(71_109.60));
    }

    #[test]
    fn target_price_can_fall_back_to_slug_scoped_price_to_beat() {
        let body = r#"
            {"ticker":"btc-updown-5m-1773996300","eventMetadata":{"priceToBeat":71109.60}}
        "#;
        let extracted = extract_target_price(
            body,
            "btc-updown-5m-1773996300",
            1_773_996_300_000,
            1_773_996_600_000,
        );
        assert_eq!(extracted, Some(71_109.60));
    }
}
