//! Clear errors for HTTP rate limits.
//!
//! Many agents share one token and one daemon, so a rate limit hit by one is hit by all. A bare
//! "HTTP 403" makes an agent retry at once; this error names the host and how long to wait so
//! agents back off instead. The text starts with `rate limited by` so it is easy to recognize.

use anyhow::{anyhow, Error};
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use reqwest::StatusCode;

/// Longest server message kept in the error, in characters.
const MAX_SERVER_MESSAGE: usize = 300;

/// Return a "rate limited" error when the response is a rate limit, otherwise `None`.
///
/// Detected: any HTTP 429, and a 403 that carries `x-ratelimit-remaining: 0` or `Retry-After`
/// (GitHub's primary and secondary limits). The wait comes from `Retry-After` (seconds or an
/// HTTP date), else `x-ratelimit-reset` / `ratelimit-reset` (Unix time or seconds).
pub fn rate_limit_error(
    status: StatusCode,
    headers: &HeaderMap,
    url: &str,
    body: &[u8],
) -> Option<Error> {
    rate_limit_error_at(status, headers, url, body, Utc::now())
}

/// Pass the response through unless it is a rate limit, in which case read its (small) body and
/// return the clear error. Use right after `send()`, before the generic status check.
pub async fn reject_rate_limited(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::FORBIDDEN {
        return Ok(response);
    }
    let headers = response.headers().clone();
    let url = response.url().to_string();
    if rate_limit_error(status, &headers, &url, b"").is_none() {
        return Ok(response);
    }
    let body = response.bytes().await.unwrap_or_default();
    Err(rate_limit_error(status, &headers, &url, &body)
        .unwrap_or_else(|| anyhow!("rate limited by {url}")))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn rate_limit_error_at(
    status: StatusCode,
    headers: &HeaderMap,
    url: &str,
    body: &[u8],
    now: DateTime<Utc>,
) -> Option<Error> {
    let remaining_zero = ["x-ratelimit-remaining", "ratelimit-remaining"]
        .iter()
        .any(|h| header_str(headers, h) == Some("0"));
    let retry_after = header_str(headers, "retry-after");
    let limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (remaining_zero || retry_after.is_some()));
    if !limited {
        return None;
    }

    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string());

    let retry_at = retry_after
        .and_then(|v| parse_retry_after(v, now))
        .or_else(|| {
            ["x-ratelimit-reset", "ratelimit-reset"]
                .iter()
                .find_map(|h| header_str(headers, h).and_then(|v| parse_reset(v, now)))
        });

    let mut msg = format!("rate limited by {host} (HTTP {})", status.as_u16());
    match retry_at {
        Some(at) => {
            let secs = (at - now).num_seconds().max(1);
            let when = if secs >= 120 {
                format!("about {} min, at", (secs + 30) / 60)
            } else {
                "at".to_string()
            };
            msg.push_str(&format!(
                ": retry after {secs} s ({when} {} UTC)",
                at.format("%Y-%m-%d %H:%M:%S")
            ));
        }
        None => msg.push_str(": the server did not say when; wait before retrying"),
    }
    if let Some(limit) = header_str(headers, "x-ratelimit-limit") {
        msg.push_str(&format!("; limit {limit} requests per window"));
    }
    msg.push_str(". Every agent using this token shares the limit, so retrying sooner fails too.");

    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if !text.is_empty() {
        let short: String = text.chars().take(MAX_SERVER_MESSAGE).collect();
        let ellipsis = if short.len() < text.len() { "..." } else { "" };
        msg.push_str(&format!(" Server said: {short}{ellipsis}"));
    }
    Some(anyhow!(msg))
}

/// `Retry-After` is either delay seconds or an HTTP date.
fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Ok(secs) = value.parse::<i64>() {
        return Some(now + chrono::Duration::seconds(secs.max(0)));
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Reset headers are Unix time (GitHub's `x-ratelimit-reset`) or delay seconds (the IETF
/// `ratelimit-reset`). Values that look like a Unix time are treated as one.
fn parse_reset(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let n = value.parse::<i64>().ok()?;
    if n >= 1_000_000_000 {
        DateTime::from_timestamp(n, 0)
    } else {
        Some(now + chrono::Duration::seconds(n.max(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn github_primary_limit_uses_reset_time() {
        let h = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-reset", "1800000754"),
        ]);
        let err = rate_limit_error_at(
            StatusCode::FORBIDDEN,
            &h,
            "https://api.github.com/repos/a/b",
            b"{\"message\":\"API rate limit exceeded\"}",
            now(),
        )
        .unwrap()
        .to_string();
        assert!(
            err.starts_with("rate limited by api.github.com (HTTP 403)"),
            "{err}"
        );
        assert!(
            err.contains("retry after 754 s (about 13 min, at "),
            "{err}"
        );
        assert!(err.contains("UTC"), "{err}");
        assert!(err.contains("limit 5000"), "{err}");
        assert!(err.contains("API rate limit exceeded"), "{err}");
    }

    #[test]
    fn retry_after_seconds_and_http_date() {
        let secs = rate_limit_error_at(
            StatusCode::TOO_MANY_REQUESTS,
            &headers(&[("retry-after", "30")]),
            "https://example.com/x",
            b"",
            now(),
        )
        .unwrap()
        .to_string();
        assert!(
            secs.contains("rate limited by example.com (HTTP 429): retry after 30 s"),
            "{secs}"
        );

        let date = (now() + chrono::Duration::seconds(90)).to_rfc2822();
        let dated = rate_limit_error_at(
            StatusCode::TOO_MANY_REQUESTS,
            &headers(&[("retry-after", &date)]),
            "https://example.com/x",
            b"",
            now(),
        )
        .unwrap()
        .to_string();
        assert!(dated.contains("retry after 90 s"), "{dated}");
    }

    #[test]
    fn delta_reset_and_unknown_wait() {
        let delta = rate_limit_error_at(
            StatusCode::TOO_MANY_REQUESTS,
            &headers(&[("ratelimit-reset", "12")]),
            "https://example.com/x",
            b"",
            now(),
        )
        .unwrap()
        .to_string();
        assert!(delta.contains("retry after 12 s"), "{delta}");

        let unknown = rate_limit_error_at(
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            "https://example.com/x",
            b"",
            now(),
        )
        .unwrap()
        .to_string();
        assert!(unknown.contains("did not say when"), "{unknown}");
    }

    #[test]
    fn plain_errors_are_not_rate_limits() {
        assert!(rate_limit_error_at(
            StatusCode::FORBIDDEN,
            &headers(&[("x-ratelimit-remaining", "4999")]),
            "https://api.github.com/x",
            b"forbidden",
            now(),
        )
        .is_none());
        assert!(rate_limit_error_at(
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            "https://example.com",
            b"",
            now()
        )
        .is_none());
    }

    #[test]
    fn long_server_message_is_truncated() {
        let body = "x".repeat(2000);
        let err = rate_limit_error_at(
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            "https://example.com",
            body.as_bytes(),
            now(),
        )
        .unwrap()
        .to_string();
        assert!(err.ends_with("..."), "{err}");
        assert!(err.len() < 600, "{}", err.len());
    }
}
