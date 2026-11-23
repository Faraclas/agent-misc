//! A minimal blocking HTTP layer. The spec download takes a `Transport`, so tests can answer requests without a
//! network.

use std::time::Duration;

pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Sent with POST, PUT and PATCH (empty when `None`); ignored for other methods.
    pub body: Option<Vec<u8>>,
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

/// The spec file is about 14 MB; this only guards against a runaway body.
const BODY_LIMIT: u64 = 256 * 1024 * 1024;

fn with_headers<B>(
    mut builder: ureq::RequestBuilder<B>,
    headers: &[(String, String)],
) -> ureq::RequestBuilder<B> {
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    builder
}

pub fn ureq_transport() -> Transport {
    // Trust the operating system's certificate store, like utcp does, so a company root CA that IT installed
    // (TLS inspection) is accepted. ureq's default is a built-in Mozilla list that ignores it.
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into();
    Box::new(move |req: &Request| {
        let (url, h) = (&req.url, &req.headers);
        let body = req.body.as_deref().unwrap_or(&[]);
        let sent = match req.method {
            "OPTIONS" => with_headers(agent.options(url), h).call(),
            "DELETE" => with_headers(agent.delete(url), h).call(),
            "POST" => with_headers(agent.post(url), h).send(body),
            "PUT" => with_headers(agent.put(url), h).send(body),
            "PATCH" => with_headers(agent.patch(url), h).send(body),
            _ => with_headers(agent.get(url), h).call(),
        };
        let mut resp = sent.map_err(|e| e.to_string())?;
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
