use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR};

type Sink<'a> = dyn FnMut(&[u8], Option<u64>) -> Result<(), String> + 'a;

const USER_AGENT: &str = concat!("PS5LanCapture/", env!("CARGO_PKG_VERSION"));

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn split_url(url: &str) -> Result<(&str, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("Only https URLs are supported: {url}"))?;
    Ok(match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    })
}

fn net_error(what: &str) -> String {
    format!(
        "Network error while {what}: {}",
        windows::core::Error::from_thread()
    )
}

/// Performs an HTTPS request (redirects are followed) and streams the body into `sink`,
/// which also receives the total size when the server sends Content-Length.
pub fn request(
    method: &str,
    url: &str,
    headers: &str,
    body: &[u8],
    sink: &mut Sink,
) -> Result<u32, String> {
    let (host, path) = split_url(url)?;
    let to_err = |e: windows::core::Error| format!("Network error: {e}");
    unsafe {
        let session = Handle(WinHttpOpen(
            &HSTRING::from(USER_AGENT),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(net_error("starting WinHTTP"));
        }
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);
        let connect = Handle(WinHttpConnect(
            session.0,
            &HSTRING::from(host),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if connect.0.is_null() {
            return Err(net_error(&format!("connecting to {host}")));
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            &HSTRING::from(method),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(net_error("creating the request"));
        }
        let header_w: Vec<u16> = headers.encode_utf16().collect();
        WinHttpSendRequest(
            request.0,
            (!header_w.is_empty()).then_some(header_w.as_slice()),
            (!body.is_empty()).then_some(body.as_ptr() as *const _),
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .map_err(to_err)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(to_err)?;

        let query_number = |flag: u32| -> Option<u32> {
            let mut value: u32 = 0;
            let mut size = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request.0,
                flag | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut value as *mut u32 as *mut _),
                &mut size,
                std::ptr::null_mut(),
            )
            .ok()
            .map(|_| value)
        };
        let status = query_number(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        let total = query_number(WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);

        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                &mut read,
            )
            .map_err(to_err)?;
            if read == 0 {
                break;
            }
            sink(&buffer[..read as usize], total)?;
        }
        Ok(status)
    }
}

/// Request whose body is collected in memory (limited to `max_bytes`).
pub fn fetch(
    method: &str,
    url: &str,
    headers: &str,
    body: &[u8],
    max_bytes: usize,
) -> Result<(u32, Vec<u8>), String> {
    let mut response = Vec::new();
    let status = request(method, url, headers, body, &mut |chunk, _| {
        response.extend_from_slice(chunk);
        if response.len() > max_bytes {
            return Err("Server response too large".into());
        }
        Ok(())
    })?;
    Ok((status, response))
}

/// Downloads `url` to `target`, reporting `(downloaded, total)` and stopping when `cancel` is set.
pub fn download(
    url: &str,
    target: &Path,
    progress: &dyn Fn(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut file = std::fs::File::create(target).map_err(|e| e.to_string())?;
    let mut done = 0u64;
    let status = request("GET", url, "", &[], &mut |chunk, total| {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".into());
        }
        file.write_all(chunk).map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        progress(done, total);
        Ok(())
    });
    let result = match status {
        Ok(200) => file.flush().map_err(|e| e.to_string()),
        Ok(code) => Err(format!("Download failed (HTTP {code})")),
        Err(e) => Err(e),
    };
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(target);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b").unwrap(),
            ("api.github.com", "/repos/a/b")
        );
        assert_eq!(
            split_url("https://example.com").unwrap(),
            ("example.com", "/")
        );
        assert!(split_url("http://example.com/").is_err());
    }
}
