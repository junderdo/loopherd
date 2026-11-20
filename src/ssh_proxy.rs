//! `loopherd ssh-proxy`: ssh's ProxyCommand for a sandboxed lead, whose only
//! way out is the sandbox's HTTP proxy.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use anyhow::{Context, Result, bail};

/// Relays stdin and stdout to `host:port`, through the HTTP proxy `proxy`
/// names (as `$HTTPS_PROXY` does) when there is one.
pub fn relay(host: &str, port: u16, proxy: Option<&str>) -> Result<()> {
    let stream = connect(host, port, proxy)?;
    let mut to_server = stream.try_clone().context("clone the connection")?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(std::net::Shutdown::Write);
    });
    let mut from_server = stream;
    std::io::copy(&mut from_server, &mut std::io::stdout().lock()).context("relay")?;
    Ok(())
}

fn connect(host: &str, port: u16, proxy: Option<&str>) -> Result<TcpStream> {
    let Some(proxy) = proxy.filter(|p| !p.is_empty()) else {
        return TcpStream::connect((host, port)).with_context(|| format!("connect {host}:{port}"));
    };
    let rest = proxy
        .strip_prefix("http://")
        .with_context(|| format!("proxy {proxy:?} isn't an http:// URL"))?
        .trim_end_matches('/');
    let (login, address) = match rest.rsplit_once('@') {
        Some((login, address)) => (Some(login), address),
        None => (None, rest),
    };
    let mut stream =
        TcpStream::connect(address).with_context(|| format!("connect proxy {address}"))?;
    let authorization = match login {
        Some(login) => format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64(login.as_bytes())
        ),
        None => String::new(),
    };
    write!(
        stream,
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n{authorization}\r\n"
    )
    .context("ask the proxy")?;
    // Byte by byte, so nothing of the tunnel is read past the proxy's answer.
    let mut reader = BufReader::with_capacity(1, stream.try_clone()?);
    let mut status = String::new();
    reader.read_line(&mut status).context("read the proxy")?;
    if status.split_whitespace().nth(1) != Some("200") {
        bail!("the proxy refused {host}:{port}: {}", status.trim());
    }
    let mut line = String::new();
    while reader.read_line(&mut line).context("read the proxy")? > 2 {
        line.clear();
    }
    Ok(stream)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            match i <= chunk.len() {
                true => out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;

    use super::*;

    /// A proxy that answers one CONNECT with `status`, then echoes `ping` as
    /// `pong`; returns the request it read.
    fn proxy(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                reader.read_line(&mut request).unwrap();
            }
            write!(stream, "HTTP/1.1 {status}\r\nVia: test\r\n\r\n").unwrap();
            let mut ping = [0; 4];
            if reader.read_exact(&mut ping).is_ok() && &ping == b"ping" {
                stream.write_all(b"pong").unwrap();
            }
            request
        });
        (address, served)
    }

    #[test]
    fn tunnels_through_the_proxy_with_its_login() {
        let (address, served) = proxy("200 Connection established");

        let mut stream = connect(
            "github.com",
            22,
            Some(&format!("http://user:pass@{address}")),
        )
        .unwrap();
        stream.write_all(b"ping").unwrap();
        let mut answer = [0; 4];
        stream.read_exact(&mut answer).unwrap();

        assert_eq!(&answer, b"pong");
        assert_eq!(
            served.join().unwrap(),
            "CONNECT github.com:22 HTTP/1.1\r\nHost: github.com:22\r\n\
             Proxy-Authorization: Basic dXNlcjpwYXNz\r\n\r\n"
        );
    }

    #[test]
    fn a_proxy_that_refuses_the_host_fails_with_its_answer() {
        let (address, _served) = proxy("403 Forbidden");

        let err = connect("example.com", 22, Some(&format!("http://{address}"))).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "the proxy refused example.com:22: HTTP/1.1 403 Forbidden"
        );
    }

    #[test]
    fn base64_pads_what_doesnt_fill_three_bytes() {
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"srt.x:y1"), "c3J0Lng6eTE=");
    }
}
