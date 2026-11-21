//! A minimal blocking HTTP layer. The archive client and the route check take a `Transport`, so tests can answer
//! requests without a network.

use std::time::Duration;

pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub struct Response {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request. `Err` means no HTTP response arrived (DNS, TLS, timeout); any status is `Ok`.
pub type Transport = Box<dyn FnMut(&Request) -> Result<Response, String>>;

/// Responses can be several MB (spec files, route lists); this only guards against a runaway body.
const BODY_LIMIT: u64 = 256 * 1024 * 1024;

pub fn ureq_transport() -> Transport {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into();
    Box::new(move |req: &Request| {
        let mut builder = match req.method {
            "OPTIONS" => agent.options(&req.url),
            _ => agent.get(&req.url),
        };
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        let mut resp = builder.call().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let body = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        Ok(Response {
            status,
            headers,
            body,
        })
    })
}
