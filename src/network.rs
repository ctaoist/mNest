use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::Context;
use futures::StreamExt;
use reqwest::{Client, Url, header, redirect::Policy};

const MAX_REDIRECTS: usize = 3;

pub async fn fetch_public_image(url: &str, max_bytes: usize) -> anyhow::Result<(String, Vec<u8>)> {
    let mut url = Url::parse(url).context("invalid remote image URL")?;
    for redirect in 0..=MAX_REDIRECTS {
        let client = client_for_public_url(&url).await?;
        let response = client.get(url.clone()).send().await?;
        if response.status().is_redirection() {
            anyhow::ensure!(redirect < MAX_REDIRECTS, "too many remote image redirects");
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("remote image redirect has no valid location")?;
            url = url
                .join(location)
                .context("invalid remote image redirect")?;
            continue;
        }
        let response = response.error_for_status()?;
        if let Some(length) = response.content_length() {
            anyhow::ensure!(
                length <= max_bytes as u64,
                "remote image exceeds size limit"
            );
        }
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or("image/jpeg")
            .trim()
            .to_ascii_lowercase();
        anyhow::ensure!(
            mime.starts_with("image/"),
            "remote URL did not return an image"
        );
        let mut bytes = Vec::with_capacity(response.content_length().unwrap_or_default() as usize);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            anyhow::ensure!(
                bytes.len().saturating_add(chunk.len()) <= max_bytes,
                "remote image exceeds size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        return Ok((mime, bytes));
    }
    anyhow::bail!("remote image redirect failed")
}

async fn client_for_public_url(url: &Url) -> anyhow::Result<Client> {
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "unsupported URL scheme"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are not allowed"
    );
    let host = url.host_str().context("remote image URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("remote image URL has no port")?;
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        anyhow::ensure!(
            is_public_ip(ip),
            "private or reserved network address is not allowed"
        );
        vec![SocketAddr::new(ip, port)]
    } else {
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .context("failed to resolve remote image host")?
            .collect::<Vec<_>>();
        anyhow::ensure!(!addresses.is_empty(), "remote image host has no addresses");
        anyhow::ensure!(
            addresses.iter().all(|address| is_public_ip(address.ip())),
            "remote image host resolves to a private or reserved address"
        );
        addresses
    };
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .redirect(Policy::none())
        .no_proxy()
        .user_agent("mNest/remote-image");
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    Ok(builder.build()?)
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4() {
        return is_public_ipv4(ipv4);
    }
    let segments = ip.segments();
    let first = segments[0];
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
        || first & 0xffc0 == 0xfec0
        || (first == 0x0064 && segments[1] == 0xff9b)
        || (first == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_and_reserved_addresses() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(value.parse().unwrap()), "{value}");
        }
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
