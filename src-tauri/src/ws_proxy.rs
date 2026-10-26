//! Local WebSocket relay for the Kuro realtime server.
//!
//! The server accepts WebSocket upgrades only from `http(s)://tauri.localhost` (the Windows WebView
//! origin) or without an Origin header; the macOS page origin `tauri://localhost` gets 403. The main
//! window rewrites WebSockets for that one host to `ws://127.0.0.1:<port>`; this relay replaces Host
//! and Origin in the upgrade request, opens TLS to the real server and then pipes bytes unchanged.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const UPSTREAM_HOST: &str = "api-wutheringwaves-wuwatappo.kurogames-global.com";
const ORIGIN: &str = "http://tauri.localhost";
const MAX_HEADER: usize = 16 * 1024;

/// Bind on loopback and serve in the background; returns the port.
pub fn start() -> std::io::Result<u16> {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    std_listener.set_nonblocking(true)?;
    let port = std_listener.local_addr()?.port();
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => return log::error!("[ws-proxy] listen failed: {e}"),
        };
        loop {
            let Ok((client, _)) = listener.accept().await else { continue };
            tauri::async_runtime::spawn(async move {
                if let Err(e) = relay(client).await {
                    log::warn!("[ws-proxy] connection ended: {e}");
                }
            });
        }
    });
    log::info!("[ws-proxy] listening on 127.0.0.1:{port}");
    Ok(port)
}

async fn relay(mut client: TcpStream) -> Result<(), String> {
    let mut buf = Vec::with_capacity(4096);
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let n = client.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("client closed before upgrade".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEADER {
            return Err("upgrade request too large".into());
        }
    };
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| "non-utf8 request head")?;
    let request = rewrite_upgrade(head)?;

    let tcp = TcpStream::connect((UPSTREAM_HOST, 443)).await.map_err(|e| e.to_string())?;
    let tls = tokio_native_tls::TlsConnector::from(native_tls::TlsConnector::new().map_err(|e| e.to_string())?);
    let mut upstream = tls.connect(UPSTREAM_HOST, tcp).await.map_err(|e| e.to_string())?;
    upstream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    upstream.write_all(&buf[header_end..]).await.map_err(|e| e.to_string())?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Keep the request line and headers, but point Host/Origin at what the server expects.
/// Only WebSocket upgrades are relayed, so this cannot be used as a general HTTP proxy.
fn rewrite_upgrade(head: &str) -> Result<String, String> {
    let mut lines = head.split("\r\n").filter(|l| !l.is_empty());
    let request_line = lines.next().ok_or("empty request")?;
    if !request_line.starts_with("GET /") {
        return Err(format!("refusing non-GET request: {request_line}"));
    }
    let mut out = format!("{request_line}\r\nHost: {UPSTREAM_HOST}\r\nOrigin: {ORIGIN}\r\n");
    let mut upgrade = false;
    for line in lines {
        let name = line.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
        if name == "host" || name == "origin" {
            continue;
        }
        if name == "upgrade" && line.to_ascii_lowercase().contains("websocket") {
            upgrade = true;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !upgrade {
        return Err("not a websocket upgrade".into());
    }
    out.push_str("\r\n");
    Ok(out)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Main-window script: send WebSockets for the Kuro host through the relay.
pub fn rewrite_script(port: u16) -> String {
    format!(
        r#"(() => {{
  const Native = window.WebSocket;
  const host = '{UPSTREAM_HOST}';
  function RelayedWebSocket(url, protocols) {{
    try {{
      const u = new URL(url, location.href);
      if (u.host === host) url = 'ws://127.0.0.1:{port}' + u.pathname + u.search;
    }} catch {{}}
    return protocols === undefined ? new Native(url) : new Native(url, protocols);
  }}
  RelayedWebSocket.prototype = Native.prototype;
  for (const k of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) RelayedWebSocket[k] = Native[k];
  window.WebSocket = RelayedWebSocket;
}})();"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_host_and_origin_only_for_websocket_get() {
        let head = "GET /socket.io/?EIO=4&transport=websocket&token=t HTTP/1.1\r\nHost: 127.0.0.1:5555\r\nOrigin: tauri://localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: k\r\n\r\n";
        let out = rewrite_upgrade(head).unwrap();
        assert!(out.starts_with("GET /socket.io/?EIO=4&transport=websocket&token=t HTTP/1.1\r\n"));
        assert!(out.contains(&format!("Host: {UPSTREAM_HOST}\r\n")));
        assert!(out.contains("Origin: http://tauri.localhost\r\n"));
        assert!(!out.contains("tauri://localhost") && !out.contains("127.0.0.1"));
        assert!(out.ends_with("Sec-WebSocket-Key: k\r\n\r\n"));
        assert!(rewrite_upgrade("POST /x HTTP/1.1\r\nUpgrade: websocket\r\n\r\n").is_err());
        assert!(rewrite_upgrade("GET /x HTTP/1.1\r\nHost: a\r\n\r\n").is_err());
    }
}
