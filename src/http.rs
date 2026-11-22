//! The backchannel download: one plain HTTP/1.1 GET a region (Chunksmith serves
//! `/lod/<dimension>/r.X.Z.cslod` beside the game port, with the token from its hello).

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_BODY: usize = 256 << 20;

pub fn get(host: &str, port: u16, path: &str, token: &str) -> io::Result<Vec<u8>> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "host did not resolve"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(8))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nX-Chunksmith-Token: {token}\r\nConnection: close\r\nUser-Agent: chunksmith-bridge\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    stream.take(MAX_BODY as u64).read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HTTP header"))?;
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(io::Error::other(format!("HTTP {status}")));
    }
    let mut body = response[split + 4..].to_vec();
    let length = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
    });
    if let Some(len) = length {
        if body.len() < len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "download cut short"));
        }
        body.truncate(len);
    }
    Ok(body)
}
