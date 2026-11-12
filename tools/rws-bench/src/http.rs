use anyhow::{Context, Result, bail};
use bytes::{Buf, BytesMut};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, TcpStream, lookup_host},
};
use url::Url;

#[derive(Debug, Clone)]
pub struct HttpEndpoint {
    pub display: String,
    pub host: String,
    pub port: u16,
    pub path_and_query: String,
}

impl HttpEndpoint {
    pub fn parse(input: &str) -> Result<Self> {
        let url = Url::parse(input).with_context(|| format!("invalid target URL '{input}'"))?;
        if url.scheme() != "http" {
            bail!("only plain HTTP is currently supported; use an http:// target");
        }
        let host = url.host_str().context("target URL has no host")?.to_owned();
        let port = url
            .port_or_known_default()
            .context("target URL has no port")?;
        let mut path_and_query = url.path().to_owned();
        if path_and_query.is_empty() {
            path_and_query.push('/');
        }
        if let Some(query) = url.query() {
            path_and_query.push('?');
            path_and_query.push_str(query);
        }
        Ok(Self {
            display: input.to_owned(),
            host,
            port,
            path_and_query,
        })
    }

    pub async fn resolve(&self) -> Result<Vec<SocketAddr>> {
        let addresses = lookup_host((self.host.as_str(), self.port))
            .await?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            bail!("target resolved to no addresses");
        }
        Ok(addresses)
    }
}

pub struct HttpConnection {
    stream: TcpStream,
    read_buf: BytesMut,
    request: Vec<u8>,
    host_header: String,
}

impl HttpConnection {
    pub async fn connect(
        address: SocketAddr,
        source_ip: Option<IpAddr>,
        endpoint: &HttpEndpoint,
        host_header: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let socket = if address.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        if let Some(ip) = source_ip {
            if ip.is_ipv4() != address.is_ipv4() {
                bail!("source IP address family does not match target");
            }
            socket.bind(SocketAddr::new(ip, 0))?;
        }
        let stream = tokio::time::timeout(timeout, socket.connect(address))
            .await
            .context("TCP connect timed out")??;
        stream.set_nodelay(true)?;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rws-bench/{}\r\nAccept: */*\r\nConnection: keep-alive\r\n\r\n",
            endpoint.path_and_query,
            host_header,
            env!("CARGO_PKG_VERSION")
        ).into_bytes();
        Ok(Self {
            stream,
            read_buf: BytesMut::with_capacity(4096),
            request,
            host_header: host_header.to_owned(),
        })
    }

    pub async fn request(&mut self, timeout: Duration) -> Result<u16> {
        tokio::time::timeout(timeout, self.stream.write_all(&self.request))
            .await
            .context("request write timed out")??;
        tokio::time::timeout(timeout, self.read_response())
            .await
            .context("response read timed out")?
    }

    pub async fn request_path(&mut self, timeout: Duration, path: &str) -> Result<u16> {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rws-bench/{}\r\nAccept: */*\r\nConnection: keep-alive\r\n\r\n",
            self.host_header,
            env!("CARGO_PKG_VERSION")
        );
        tokio::time::timeout(timeout, self.stream.write_all(request.as_bytes()))
            .await
            .context("request write timed out")??;
        tokio::time::timeout(timeout, self.read_response())
            .await
            .context("response read timed out")?
    }

    async fn read_response(&mut self) -> Result<u16> {
        loop {
            if let Some((status, consumed)) = complete_response(&self.read_buf)? {
                self.read_buf.advance(consumed);
                return Ok(status);
            }
            let read = self.stream.read_buf(&mut self.read_buf).await?;
            if read == 0 {
                bail!("connection closed before a complete response");
            }
            if self.read_buf.len() > 16 * 1024 * 1024 {
                bail!("response exceeded the 16 MiB safety limit");
            }
        }
    }
}

fn complete_response(buffer: &[u8]) -> Result<Option<(u16, usize)>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    let header_len = match response.parse(buffer)? {
        httparse::Status::Partial => return Ok(None),
        httparse::Status::Complete(length) => length,
    };
    let status = response.code.context("HTTP response has no status")?;
    let mut content_length = None;
    let mut chunked = false;
    for header in response.headers {
        if header.name.eq_ignore_ascii_case("content-length") {
            let text = std::str::from_utf8(header.value)?.trim();
            content_length = Some(text.parse::<usize>().context("invalid Content-Length")?);
        }
        if header.name.eq_ignore_ascii_case("transfer-encoding")
            && std::str::from_utf8(header.value)?
                .to_ascii_lowercase()
                .contains("chunked")
        {
            chunked = true;
        }
    }
    if chunked {
        return Ok(chunked_len(&buffer[header_len..])?.map(|body| (status, header_len + body)));
    }
    let length = content_length.context(
        "response has neither Content-Length nor chunked framing; persistent reuse is unsafe",
    )?;
    let total = header_len.saturating_add(length);
    Ok((buffer.len() >= total).then_some((status, total)))
}

fn chunked_len(body: &[u8]) -> Result<Option<usize>> {
    let mut offset = 0usize;
    loop {
        let Some(line_end) = body[offset..].windows(2).position(|value| value == b"\r\n") else {
            return Ok(None);
        };
        let line_end = offset + line_end;
        let size_text = std::str::from_utf8(&body[offset..line_end])?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let size = usize::from_str_radix(size_text, 16).context("invalid chunk size")?;
        offset = line_end + 2;
        if size == 0 {
            let Some(trailer_end) = body[offset..]
                .windows(4)
                .position(|value| value == b"\r\n\r\n")
            else {
                if body.get(offset..offset + 2) == Some(b"\r\n") {
                    return Ok(Some(offset + 2));
                }
                return Ok(None);
            };
            return Ok(Some(offset + trailer_end + 4));
        }
        let end = offset.saturating_add(size);
        if body.len() < end + 2 {
            return Ok(None);
        }
        if &body[end..end + 2] != b"\r\n" {
            bail!("invalid chunk terminator");
        }
        offset = end + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_and_preserves_pipeline_tail() {
        let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokTAIL";
        assert_eq!(
            complete_response(bytes).unwrap(),
            Some((200, bytes.len() - 4))
        );
    }

    #[test]
    fn parses_chunked_response() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n";
        assert_eq!(complete_response(bytes).unwrap(), Some((200, bytes.len())));
    }
}
