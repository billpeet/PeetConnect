//! Minimal HTTPS GET over WinHTTP: the system's TLS stack, certificate store and proxy
//! settings, and next to nothing added to the binaries. Redirects (GitHub sends release
//! downloads to its asset host) are followed by WinHTTP itself, never from HTTPS to HTTP.

use std::ffi::c_void;

use anyhow::{bail, Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

/// An open WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &str) -> Result<Self> {
        if raw.is_null() {
            return Err(windows::core::Error::from_thread()).context(what.to_string());
        }
        Ok(Self(raw))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle returned by WinHTTP, closed exactly once.
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

struct Url<'a> {
    secure: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse(url: &str) -> Result<Url<'_>> {
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        bail!("unsupported URL {url}");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().context("bad port in URL")?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    anyhow::ensure!(!host.is_empty(), "no host in {url}");
    Ok(Url {
        secure,
        host,
        port,
        path,
    })
}

/// Fetches `url`, handing the body to `sink` in chunks as it arrives. Fails on any status but
/// 200.
pub fn fetch(url: &str, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let parts = parse(url)?;
    let agent = HSTRING::from(format!("PeetConnect/{}", crate::VERSION));
    let host = HSTRING::from(parts.host);
    let path = HSTRING::from(parts.path);
    // SAFETY: WinHTTP calls with valid NUL-terminated strings; every handle is owned by a
    // `Handle` and closed in reverse order on return.
    unsafe {
        let session = Handle::new(
            WinHttpOpen(
                &agent,
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "WinHttpOpen",
        )?;
        // Resolve, connect, send, receive: generous for slow links, finite so a check never
        // hangs forever.
        WinHttpSetTimeouts(session.0, 30_000, 30_000, 30_000, 60_000)
            .context("WinHttpSetTimeouts")?;
        let connection = Handle::new(
            WinHttpConnect(session.0, &host, parts.port, 0),
            "connecting",
        )?;
        let flags = if parts.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(
            WinHttpOpenRequest(
                connection.0,
                &HSTRING::from("GET"),
                &path,
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            ),
            "WinHttpOpenRequest",
        )?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0)
            .with_context(|| format!("requesting {url}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .with_context(|| format!("waiting for {url}"))?;

        let mut status = 0u32;
        let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut size,
            std::ptr::null_mut(),
        )
        .context("reading the HTTP status")?;
        match status {
            200 => {}
            404 => bail!("{url}: not found (HTTP 404)"),
            other => bail!("{url}: HTTP {other}"),
        }

        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                &mut read,
            )
            .with_context(|| format!("downloading {url}"))?;
            if read == 0 {
                return Ok(());
            }
            sink(&buffer[..read as usize])?;
        }
    }
}

/// Fetches a small document whole, failing if it is larger than `limit` bytes.
pub fn get(url: &str, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    fetch(url, |chunk| {
        anyhow::ensure!(
            body.len() + chunk.len() <= limit,
            "{url} is larger than {limit} bytes"
        );
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = parse("https://github.com/a/b?c=d").unwrap();
        assert!(u.secure);
        assert_eq!((u.host, u.port, u.path), ("github.com", 443, "/a/b?c=d"));
        let u = parse("http://127.0.0.1:8000").unwrap();
        assert!(!u.secure);
        assert_eq!((u.host, u.port, u.path), ("127.0.0.1", 8000, "/"));
        assert!(parse("ftp://x/y").is_err());
        assert!(parse("https:///y").is_err());
    }
}
