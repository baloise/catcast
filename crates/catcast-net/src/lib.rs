use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    client_async_tls_with_config, connect_async,
    tungstenite::{client::IntoClientRequest, handshake::client::Response, http::Uri},
    MaybeTlsStream, WebSocketStream,
};
use url::Url;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to a ws:// or wss:// URL, honoring common proxy env vars
/// (`WSS_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`HTTP_PROXY`, plus lowercase
/// variants) and `NO_PROXY` bypass rules.
pub async fn connect(url: &str) -> Result<(WsStream, Response)> {
    let request = url
        .into_client_request()
        .with_context(|| format!("build websocket request for {url}"))?;
    let uri = request.uri().clone();

    let Some(proxy_raw) = proxy_for_uri(&uri) else {
        return connect_async(url)
            .await
            .with_context(|| format!("connect directly to {url}"));
    };

    let proxy = ProxyConfig::parse(&proxy_raw)
        .with_context(|| format!("parse proxy URL from environment: {proxy_raw}"))?;
    let authority = target_authority(&uri)?;

    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .with_context(|| format!("connect to proxy {}:{}", proxy.host, proxy.port))?;

    let mut connect_req = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
    );
    if let Some(auth) = proxy.basic_auth_header() {
        connect_req.push_str(&format!("Proxy-Authorization: Basic {auth}\r\n"));
    }
    connect_req.push_str("\r\n");
    stream
        .write_all(connect_req.as_bytes())
        .await
        .context("send CONNECT request to proxy")?;

    let status_line = read_connect_status_line(&mut stream).await?;
    if !status_line.contains(" 200 ") {
        return Err(anyhow!("proxy CONNECT failed: {status_line}"));
    }

    client_async_tls_with_config(request, stream, None, None)
        .await
        .with_context(|| format!("websocket handshake via proxy {proxy_raw}"))
}

async fn read_connect_status_line(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 512];
    let mut header_end = None;

    while header_end.is_none() {
        let n = stream
            .read(&mut chunk)
            .await
            .context("read CONNECT response from proxy")?;
        if n == 0 {
            return Err(anyhow!("proxy closed connection during CONNECT"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 16 * 1024 {
            return Err(anyhow!("proxy CONNECT response headers too large"));
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = Some(pos + 4);
        }
    }

    let end = header_end.expect("header_end is set");
    let header = std::str::from_utf8(&buf[..end]).context("proxy CONNECT response is not UTF-8")?;
    let line = header.lines().next().unwrap_or_default().trim().to_string();
    if line.is_empty() {
        return Err(anyhow!("proxy CONNECT response missing status line"));
    }
    Ok(line)
}

fn target_authority(uri: &Uri) -> Result<String> {
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("websocket URL is missing host"))?;
    let scheme = uri.scheme_str().unwrap_or("wss");
    let port = uri.port_u16().unwrap_or_else(|| {
        if scheme.eq_ignore_ascii_case("ws") {
            80
        } else {
            443
        }
    });
    Ok(format!("{host}:{port}"))
}

fn proxy_for_uri(uri: &Uri) -> Option<String> {
    let host = uri.host()?;
    let scheme = uri.scheme_str().unwrap_or("wss");
    let port = uri.port_u16().unwrap_or_else(|| {
        if scheme.eq_ignore_ascii_case("ws") {
            80
        } else {
            443
        }
    });

    if bypass_proxy(host, port) {
        return None;
    }

    let candidates: &[&str] = if scheme.eq_ignore_ascii_case("ws") {
        &["WS_PROXY", "ALL_PROXY", "HTTP_PROXY"]
    } else {
        &["WSS_PROXY", "HTTPS_PROXY", "ALL_PROXY", "HTTP_PROXY"]
    };
    for key in candidates {
        if let Some(v) = env_var_ci(key) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn bypass_proxy(host: &str, port: u16) -> bool {
    let Some(raw) = env_var_ci("NO_PROXY") else {
        return false;
    };
    raw.split(',').map(str::trim).any(|entry| {
        if entry.is_empty() {
            return false;
        }
        if entry == "*" {
            return true;
        }
        let (token_host, token_port) = split_host_port_token(entry);
        if let Some(tp) = token_port {
            if tp != port {
                return false;
            }
        }
        let token = token_host.trim_start_matches('.');
        host.eq_ignore_ascii_case(token)
            || host
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", token.to_ascii_lowercase()))
    })
}

fn split_host_port_token(token: &str) -> (&str, Option<u16>) {
    let Some((h, p)) = token.rsplit_once(':') else {
        return (token, None);
    };
    match p.parse::<u16>() {
        Ok(port) => (h, Some(port)),
        Err(_) => (token, None),
    }
}

fn env_var_ci(name: &str) -> Option<String> {
    std::env::vars_os().find_map(|(k, v)| {
        if k.to_string_lossy().eq_ignore_ascii_case(name) {
            Some(v.to_string_lossy().into_owned())
        } else {
            None
        }
    })
}

struct ProxyConfig {
    host: String,
    port: u16,
    user: Option<String>,
    pass: Option<String>,
}

impl ProxyConfig {
    fn parse(raw: &str) -> Result<Self> {
        let normalized = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("http://{raw}")
        };
        let url =
            Url::parse(&normalized).with_context(|| format!("invalid proxy URL: {normalized}"))?;
        if !url.scheme().eq_ignore_ascii_case("http") {
            return Err(anyhow!(
                "unsupported proxy scheme {:?}; only http:// proxies are supported",
                url.scheme()
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("proxy URL missing host: {normalized}"))?
            .to_string();
        let port = url.port_or_known_default().unwrap_or(80);
        let user = (!url.username().is_empty()).then(|| url.username().to_string());
        let pass = url.password().map(ToOwned::to_owned);
        Ok(Self {
            host,
            port,
            user,
            pass,
        })
    }

    fn basic_auth_header(&self) -> Option<String> {
        let user = self.user.as_ref()?;
        let mut plain = user.clone();
        plain.push(':');
        plain.push_str(self.pass.as_deref().unwrap_or(""));
        Some(general_purpose::STANDARD.encode(plain.as_bytes()))
    }
}
