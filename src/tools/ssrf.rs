//! The guard on requests a tool makes for the model: the host is resolved here, every
//! address it resolves to must be public, and the connection goes to the address that was
//! checked, so a DNS answer that changes between the check and the connect cannot point
//! the request inside. Redirects are followed by hand and each hop goes through it again.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::{Method, Response, Url};

pub const MAX_REDIRECTS: usize = 5;

/// Why a request may not reach `ip`, or `None` when it is a public address.
pub fn refusal(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(ip) => refusal_v4(ip),
        IpAddr::V6(ip) => refusal_v6(ip),
    }
}

fn refusal_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, c, _] = ip.octets();
    Some(match (a, b) {
        _ if ip.is_unspecified() => "unspecified",
        (0, _) => "this-network",
        (127, _) => "loopback",
        (10, _) | (172, 16..=31) | (192, 168) => "private",
        (100, 64..=127) => "shared (CGNAT)",
        (169, 254) => "link-local",
        (192, 0) if c == 0 => "IETF protocol",
        (198, 18..=19) => "benchmarking",
        (224..=239, _) => "multicast",
        (240..=255, _) => "reserved",
        _ => return None,
    })
}

fn refusal_v6(ip: Ipv6Addr) -> Option<&'static str> {
    if let Some(v4) = embedded_v4(ip) {
        return refusal_v4(v4);
    }
    let first = ip.segments()[0];
    Some(match first {
        _ if ip.is_unspecified() => "unspecified",
        _ if ip.is_loopback() => "loopback",
        0xfe80..=0xfebf => "link-local",
        0xfec0..=0xfeff => "site-local",
        0xfc00..=0xfdff => "unique local",
        0xff00..=0xffff => "multicast",
        0x0100 if ip.segments()[1..4] == [0; 3] => "discard",
        _ => return None,
    })
}

/// The IPv4 address an IPv6 one reaches: mapped (`::ffff:a.b.c.d`), compatible
/// (`::a.b.c.d`), NAT64 (`64:ff9b::/96`) and 6to4 (`2002::/16`).
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let low = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    match s {
        [0, 0, 0, 0, 0, 0xffff, _, _] => Some(low),
        [0, 0, 0, 0, 0, 0, _, _] if !ip.is_unspecified() && !ip.is_loopback() => Some(low),
        [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Some(low),
        [0x2002, hi, lo, ..] => Some(Ipv4Addr::new(
            (hi >> 8) as u8,
            hi as u8,
            (lo >> 8) as u8,
            lo as u8,
        )),
        _ => None,
    }
}

/// The address `url` connects to, after checking that it is http(s) and that every
/// address its host resolves to is public. One bad record refuses the host, since which
/// record a connection picks is not ours to control.
pub async fn check(url: &Url) -> Result<SocketAddr, String> {
    check_with(url, refusal).await
}

async fn check_with(
    url: &Url,
    refusal: fn(IpAddr) -> Option<&'static str>,
) -> Result<SocketAddr, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{url}: only http and https are fetched"));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| format!("{url}: no port"))?;
    let host = url.host_str().ok_or_else(|| format!("{url}: no host"))?;
    let addrs: Vec<SocketAddr> = match literal(host) {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("{host}: does not resolve: {e}"))?
            .collect(),
    };
    for addr in &addrs {
        if let Some(why) = refusal(addr.ip()) {
            return Err(format!("refused: {host} is {}, a {why} address", addr.ip()));
        }
    }
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| format!("{url}: resolves to no address"))
}

/// The address a URL host names directly; the url crate has already normalised forms
/// like `2130706433` and `0x7f.1`, and keeps an IPv6 host in brackets.
pub fn literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// A client for one request to `url` that connects only to `addr`. No proxy, since a
/// proxy would resolve the host again, and no redirects, so the caller checks each hop.
pub fn pinned(url: &Url, addr: SocketAddr, timeout: Duration) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent(concat!("bhai/", env!("CARGO_PKG_VERSION")));
    if let Some(host) = url.host_str().filter(|host| literal(host).is_none()) {
        builder = builder.resolve(host, addr);
    }
    builder.build().map_err(|e| format!("{e}"))
}

/// `method` on `url` through the guard, following up to `MAX_REDIRECTS` redirects and
/// checking each. The response's `url()` is the final hop. `timeout` is per hop.
pub async fn send(method: Method, url: Url, timeout: Duration) -> Result<Response, String> {
    send_with(method, url, timeout, refusal).await
}

pub(super) async fn send_with(
    method: Method,
    mut url: Url,
    timeout: Duration,
    refusal: fn(IpAddr) -> Option<&'static str>,
) -> Result<Response, String> {
    for _ in 0..=MAX_REDIRECTS {
        let addr = check_with(&url, refusal).await?;
        let response = pinned(&url, addr, timeout)?
            .request(method.clone(), url.clone())
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
            return Ok(response);
        };
        let location = location
            .to_str()
            .map_err(|_| format!("{url}: a redirect to a location that is not text"))?;
        url = url
            .join(location)
            .map_err(|e| format!("{url}: a redirect to {location}: {e}"))?;
    }
    Err(format!(
        "more than {MAX_REDIRECTS} redirects, the last to {url}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn the_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("0.0.0.0", Some("unspecified")),
            ("0.1.2.3", Some("this-network")),
            ("127.0.0.1", Some("loopback")),
            ("127.255.255.254", Some("loopback")),
            ("10.0.0.1", Some("private")),
            ("172.16.0.1", Some("private")),
            ("172.31.255.255", Some("private")),
            ("172.32.0.1", None),
            ("192.168.1.1", Some("private")),
            ("100.64.0.1", Some("shared (CGNAT)")),
            ("100.127.255.255", Some("shared (CGNAT)")),
            ("100.128.0.1", None),
            ("169.254.169.254", Some("link-local")),
            ("192.0.0.170", Some("IETF protocol")),
            ("198.18.0.1", Some("benchmarking")),
            ("224.0.0.1", Some("multicast")),
            ("255.255.255.255", Some("reserved")),
            ("8.8.8.8", None),
            ("93.184.216.34", None),
            ("::", Some("unspecified")),
            ("::1", Some("loopback")),
            ("fe80::1", Some("link-local")),
            ("febf::1", Some("link-local")),
            ("fec0::1", Some("site-local")),
            ("fc00::1", Some("unique local")),
            ("fd00:ec2::254", Some("unique local")),
            ("ff02::1", Some("multicast")),
            ("100::1", Some("discard")),
            ("::ffff:127.0.0.1", Some("loopback")),
            ("::ffff:169.254.169.254", Some("link-local")),
            ("::ffff:8.8.8.8", None),
            ("::10.0.0.1", Some("private")),
            ("64:ff9b::a9fe:a9fe", Some("link-local")),
            ("2002:c0a8:0101::1", Some("private")),
            ("2002:0808:0808::1", None),
            ("2606:4700:4700::1111", None),
            ("2001:4860:4860::8888", None),
        ];
        for (ip, want) in cases {
            assert_eq!(refusal(ip.parse().unwrap()), *want, "{ip}");
        }
    }

    #[tokio::test]
    async fn an_address_literal_is_checked_without_a_lookup() {
        let cases = [
            "http://127.0.0.1/",
            "http://[::1]:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::ffff:a9fe:a9fe]/",
            "http://2130706433/",
            "http://0x7f.1/",
            "https://10.0.0.1/",
        ];
        for url in cases {
            let error = check(&Url::parse(url).unwrap()).await.unwrap_err();
            assert!(error.starts_with("refused:"), "{url}: {error}");
        }
        let addr = check(&Url::parse("http://8.8.8.8/").unwrap())
            .await
            .unwrap();
        assert_eq!(addr, "8.8.8.8:80".parse().unwrap());
    }

    #[tokio::test]
    async fn only_http_and_https() {
        for url in ["file:///etc/passwd", "ftp://8.8.8.8/", "gopher://8.8.8.8/"] {
            let error = check(&Url::parse(url).unwrap()).await.unwrap_err();
            assert!(error.contains("only http and https"), "{url}: {error}");
        }
    }

    #[tokio::test]
    async fn localhost_is_refused_after_the_lookup() {
        let error = check(&Url::parse("http://localhost:9/").unwrap())
            .await
            .unwrap_err();
        assert!(error.contains("loopback"), "{error}");
    }

    /// Answers each connection with the next canned response, and returns where it listens.
    async fn serve(responses: Vec<String>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        addr
    }

    fn redirect(to: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\n\r\n")
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// The real guard with 127.0.0.1 let through, so a local server can stand in.
    fn but_local(ip: IpAddr) -> Option<&'static str> {
        if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return None;
        }
        refusal(ip)
    }

    #[tokio::test]
    async fn the_connection_goes_to_the_checked_address() {
        let addr = serve(vec![ok("pinned")]).await;
        let url = Url::parse(&format!("http://pinned.invalid:{}/", addr.port())).unwrap();
        let response = pinned(&url, addr, Duration::from_secs(5))
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "pinned");
    }

    #[tokio::test]
    async fn each_redirect_hop_is_checked() {
        let addr = serve(vec![redirect("http://127.0.0.2/"), ok("inside")]).await;
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let error = send_with(Method::GET, url, Duration::from_secs(5), but_local)
            .await
            .unwrap_err();
        assert!(
            error.contains("127.0.0.2") && error.contains("loopback"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_relative_redirect_is_followed() {
        let addr = serve(vec![redirect("/next"), ok("arrived")]).await;
        let url = Url::parse(&format!("http://{addr}/start")).unwrap();
        let response = send_with(Method::GET, url, Duration::from_secs(5), but_local)
            .await
            .unwrap();
        assert_eq!(response.url().path(), "/next");
        assert_eq!(response.text().await.unwrap(), "arrived");
    }

    #[tokio::test]
    async fn redirects_stop_after_five() {
        let addr = serve((0..=MAX_REDIRECTS).map(|_| redirect("/again")).collect()).await;
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let error = send_with(Method::GET, url, Duration::from_secs(5), but_local)
            .await
            .unwrap_err();
        assert!(error.starts_with("more than 5 redirects"), "{error}");
    }
}
