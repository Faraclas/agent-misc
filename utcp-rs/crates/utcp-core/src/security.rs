//! Security utilities for validating inputs and preventing common vulnerabilities.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Validates that a file path is safe and doesn't allow directory traversal.
///
/// # Arguments
/// * `path` - The path to validate
/// * `allowed_base` - Optional base directory that the path must be within
///
/// # Returns
/// Canonicalized path if valid, error otherwise
pub fn validate_file_path(path: &str, allowed_base: Option<&str>) -> Result<PathBuf> {
    let path_buf = PathBuf::from(path);

    // Prevent absolute paths from escaping the base
    if let Some(base) = allowed_base {
        let canon_path = std::fs::canonicalize(&path_buf)
            .map_err(|e| anyhow!("Failed to canonicalize path '{}': {}", path, e))?;

        let canon_base = std::fs::canonicalize(base)
            .map_err(|e| anyhow!("Failed to canonicalize base '{}': {}", base, e))?;

        if !canon_path.starts_with(canon_base) {
            return Err(anyhow!(
                "Path '{}' is outside allowed directory '{}'",
                path,
                base
            ));
        }

        Ok(canon_path)
    } else {
        // Just canonicalize without base restriction
        std::fs::canonicalize(&path_buf).map_err(|e| anyhow!("Invalid path '{}': {}", path, e))
    }
}

/// Validates a command name against an allowlist.
/// This helps prevent command injection attacks.
///
/// # Arguments
/// * `command` - The command to validate
/// * `allowed_commands` - List of permitted command names or paths
///
/// # Returns
/// Ok if command is in allowlist, error otherwise
pub fn validate_command(command: &str, allowed_commands: &[&str]) -> Result<()> {
    // Check for shell metacharacters that could enable injection. The command is started directly, not through a
    // shell; on Windows `\`, `(` and `)` are ordinary path characters (`C:\Program Files (x86)\nodejs\node.exe`).
    #[cfg(not(windows))]
    const DANGEROUS_CHARS: &[char] = &[
        '|', '&', ';', '\n', '`', '$', '(', ')', '<', '>', '"', '\'', '\\',
    ];
    #[cfg(windows)]
    const DANGEROUS_CHARS: &[char] = &['|', '&', ';', '\n', '`', '$', '<', '>', '"', '\''];

    if command.chars().any(|c| DANGEROUS_CHARS.contains(&c)) {
        return Err(anyhow!(
            "Command contains dangerous characters: '{}'",
            command
        ));
    }

    // Extract just the command name (first component of path)
    let path_buf = PathBuf::from(command);
    let cmd_name = path_buf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);

    // Check against allowlist
    if !allowed_commands.is_empty() && !allowed_commands.contains(&cmd_name) {
        return Err(anyhow!(
            "Command '{}' is not in the allowed list. Permitted commands: {:?}",
            cmd_name,
            allowed_commands
        ));
    }

    Ok(())
}

/// Validates command arguments for potentially dangerous content.
///
/// # Arguments
/// * `args` - The arguments to validate
///
/// # Returns
/// Ok if arguments appear safe, error otherwise
pub fn validate_command_args(args: &[String]) -> Result<()> {
    for arg in args {
        // Check for shell injection patterns
        if arg.contains("&&") || arg.contains("||") || arg.contains(";") || arg.contains("|") {
            return Err(anyhow!(
                "Argument contains dangerous shell operators: '{}'",
                arg
            ));
        }

        // Check for command substitution
        if arg.contains("$(") || arg.contains("`") {
            return Err(anyhow!("Argument contains command substitution: '{}'", arg));
        }
    }

    Ok(())
}

/// Validates that a URL is safe from cloud metadata SSRF while permitting direct
/// user-configured localhost, LAN, and private hostnames (e.g., `nas`, `media-server`).
pub fn validate_url_security(url: &str, require_tls: bool) -> Result<()> {
    let url_lower = url.to_lowercase();

    if require_tls
        && !(url_lower.starts_with("https://")
            || url_lower.starts_with("wss://")
            || url_lower.starts_with("grpcs://"))
    {
        return Err(anyhow!(
            "URL must use TLS/SSL (https://, wss://, grpcs://): '{}'",
            url
        ));
    }

    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            if is_blocked_metadata_host(host) {
                return Err(anyhow!(
                    "SSRF protection blocked cloud metadata endpoint: '{}'",
                    url
                ));
            }
        }
    }

    Ok(())
}

/// Returns true if the host is a cloud instance metadata address that must always be blocked.
pub fn is_blocked_metadata_host(host: &str) -> bool {
    let trimmed = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if matches!(
        trimmed.as_str(),
        "169.254.169.254"
            | "169.254.170.2"
            | "fd00:ec2::254"
            | "metadata.google.internal"
            | "metadata.goog"
            | "100.100.100.200"
    ) {
        return true;
    }
    if let Ok(std::net::IpAddr::V4(v4)) = trimmed.parse::<std::net::IpAddr>() {
        // Link-local 169.254.0.0/16 metadata range
        let octets = v4.octets();
        if octets[0] == 169 && octets[1] == 254 {
            return true;
        }
    }
    false
}

/// Returns true if a URL host is loopback, RFC1918 private LAN, Tailscale CGNAT, or single-label local host.
pub fn is_private_or_local_host(host: &str) -> bool {
    let trimmed = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if trimmed == "localhost" || trimmed.ends_with(".local") || !trimmed.contains('.') {
        return true;
    }
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || (o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000)
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        };
    }
    false
}

/// Builds a shared `reqwest::Client` using `rustls` native OS roots and redirect-SSRF protection:
/// - Blocks redirects to cloud metadata endpoints (`169.254.169.254`, etc.)
/// - Blocks redirects from public hosts to private/loopback/local hosts
/// - Allows direct user-configured `localhost` / LAN / local hostnames (`nas`, `media-server`)
pub fn build_secure_http_client() -> reqwest::Client {
    let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let next_url = attempt.url();
        if let Some(next_host) = next_url.host_str() {
            if is_blocked_metadata_host(next_host) {
                return attempt.error("SSRF protection blocked redirect to metadata host");
            }
            if let Some(first_url) = attempt.previous().first() {
                if let Some(first_host) = first_url.host_str() {
                    if !is_private_or_local_host(first_host) && is_private_or_local_host(next_host)
                    {
                        return attempt.error(
                            "SSRF protection blocked redirect from public host to private/local host",
                        );
                    }
                }
            }
        }
        attempt.follow()
    });

    // The 60 s total timeout bounds the other transports. HTTP tool calls replace it per request with a
    // stall timeout (`transports::http::STALL_TIMEOUT`), so large uploads and downloads can take as long
    // as they keep moving.
    let builder = reqwest::Client::builder()
        .user_agent(concat!("utcp-rs/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect_policy)
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(100)
        .pool_idle_timeout(Some(std::time::Duration::from_secs(90)))
        .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
        .gzip(true);
    http2_settings(builder)
        .build()
        .expect("Failed to build secure rustls HTTP client")
}

/// HTTP/2 settings of the shared client, separate so tests can use them over plain TCP.
///
/// Fixed windows instead of `http2_adaptive_window`: h2 lets at most half the connection window's worth of small
/// DATA frames wait unread, and the adaptive window starts at 64 KiB, so a server sending many tiny frames (Azure
/// DevOps does) had the connection closed with "too_many_data_frames" (issue 021). 8 MiB keeps that limit, now at
/// 4 MiB, and lets a large answer arrive in few round trips.
const HTTP2_CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;
const HTTP2_STREAM_WINDOW: u32 = 4 * 1024 * 1024;

fn http2_settings(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .http2_initial_connection_window_size(HTTP2_CONNECTION_WINDOW)
        .http2_initial_stream_window_size(HTTP2_STREAM_WINDOW)
        .http2_keep_alive_interval(Some(std::time::Duration::from_secs(10)))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(20))
        .http2_keep_alive_while_idle(true)
}

/// Validates the size of input data to prevent DoS attacks.
///
/// # Arguments
/// * `data` - The data to check
/// * `max_size` - Maximum allowed size in bytes
///
/// # Returns
/// Ok if data is within limits, error otherwise
pub fn validate_size_limit(data: &[u8], max_size: usize) -> Result<()> {
    if data.len() > max_size {
        return Err(anyhow!(
            "Data size {} bytes exceeds maximum allowed size {} bytes",
            data.len(),
            max_size
        ));
    }

    Ok(())
}

/// Validates a timeout value to ensure it's reasonable.
///
/// # Arguments
/// * `timeout_ms` - Timeout in milliseconds
/// * `max_timeout_ms` - Maximum allowed timeout
///
/// # Returns
/// Ok if timeout is within limits, error otherwise
pub fn validate_timeout(timeout_ms: u64, max_timeout_ms: u64) -> Result<()> {
    if timeout_ms == 0 {
        return Err(anyhow!("Timeout cannot be zero"));
    }

    if timeout_ms > max_timeout_ms {
        return Err(anyhow!(
            "Timeout {}ms exceeds maximum allowed {}ms",
            timeout_ms,
            max_timeout_ms
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Azure DevOps sends large responses as thousands of DATA frames, many of them tiny. h2 (0.4.17+) caps how
    /// many small frames may wait unread, at half the connection window; with the adaptive window that starts at
    /// 64 KiB, a burst ended the connection ("too_many_data_frames"). Issue 021.
    ///
    /// Each unread frame under 256 bytes costs 256 minus its length, so 10,000 one-byte frames cost about 2.55 MB:
    /// over the old 32 KB limit (about 128 frames), within the new 4 MiB one even if nothing is read before the
    /// burst ends. 20,000 frames (5.1 MB) passed on Linux only because the reader kept up, and failed on Windows.
    #[tokio::test]
    async fn http2_client_reads_a_body_of_many_tiny_data_frames() {
        const FRAMES: usize = 10_000;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut conn = h2::server::handshake(socket).await.unwrap();
            while let Some(Ok((_request, mut respond))) = conn.accept().await {
                let mut send = respond
                    .send_response(http::Response::new(()), false)
                    .unwrap();
                tokio::spawn(async move {
                    for _ in 0..FRAMES {
                        send.send_data(bytes::Bytes::from_static(b"x"), false)
                            .unwrap();
                    }
                    send.send_data(bytes::Bytes::new(), true).unwrap();
                });
            }
        });

        let client = http2_settings(reqwest::Client::builder().http2_prior_knowledge())
            .build()
            .unwrap();
        let body = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .expect("the whole body arrives");
        assert_eq!(body.len(), FRAMES);
    }

    #[test]
    fn test_validate_command_rejects_dangerous_chars() {
        let allowed = vec!["python3", "node"];

        assert!(validate_command("python3", &allowed).is_ok());
        assert!(validate_command("ls; rm -rf /", &[]).is_err());
        assert!(validate_command("cat /etc/passwd | grep root", &[]).is_err());
        assert!(validate_command("echo `whoami`", &[]).is_err());
        assert!(validate_command("cmd && evil", &[]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn test_validate_command_accepts_windows_paths() {
        assert!(validate_command(r"C:\Program Files (x86)\nodejs\node.exe", &[]).is_ok());
        assert!(validate_command(r"C:\Users\Jane Doe\node\node.exe", &[]).is_ok());
        assert!(validate_command(r"C:\tools\a&b.exe", &[]).is_err());
    }

    #[test]
    fn test_validate_command_allowlist() {
        let allowed = vec!["python3", "node", "npm"];

        assert!(validate_command("python3", &allowed).is_ok());
        assert!(validate_command("node", &allowed).is_ok());
        assert!(validate_command("bash", &allowed).is_err());
        assert!(validate_command("/usr/bin/python3", &allowed).is_ok()); // Path is ok if basename matches
    }

    #[test]
    fn test_validate_command_args() {
        assert!(validate_command_args(&["--help".to_string()]).is_ok());
        assert!(validate_command_args(&["-v".to_string(), "file.txt".to_string()]).is_ok());

        assert!(validate_command_args(&["arg && evil".to_string()]).is_err());
        assert!(validate_command_args(&["$(whoami)".to_string()]).is_err());
        assert!(validate_command_args(&["`id`".to_string()]).is_err());
        assert!(validate_command_args(&["arg | grep".to_string()]).is_err());
    }

    #[test]
    fn test_validate_url_security() {
        assert!(validate_url_security("https://api.example.com", true).is_ok());
        assert!(validate_url_security("wss://ws.example.com", true).is_ok());
        assert!(validate_url_security("http://api.example.com", true).is_err());
        assert!(validate_url_security("http://api.example.com", false).is_ok());
    }

    #[test]
    fn test_validate_size_limit() {
        let data = vec![0u8; 1000];
        assert!(validate_size_limit(&data, 2000).is_ok());
        assert!(validate_size_limit(&data, 500).is_err());
    }

    #[test]
    fn test_validate_timeout() {
        assert!(validate_timeout(1000, 60000).is_ok());
        assert!(validate_timeout(0, 60000).is_err());
        assert!(validate_timeout(100000, 60000).is_err());
    }

    #[test]
    fn test_validate_file_path() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a test file
        let test_file = temp_path.join("test.txt");
        fs::write(&test_file, b"test").unwrap();

        // Valid path within base
        let result = validate_file_path(
            test_file.to_str().unwrap(),
            Some(temp_path.to_str().unwrap()),
        );
        assert!(result.is_ok());

        // Path outside base should fail
        let outside_path = "/tmp/outside.txt";
        let result = validate_file_path(outside_path, Some(temp_path.to_str().unwrap()));
        // This will fail because /tmp/outside.txt doesn't exist or is outside temp_dir
        assert!(result.is_err());
    }
}
