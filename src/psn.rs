use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR};

// Public OAuth client of Sony's official PS Remote Play for Windows, as used by chiaki-ng
// (scripts/psn-account-id.py). The login itself happens on Sony's own page in the browser:
// this application never sees the PlayStation password.
const CLIENT_ID: &str = "ba495a24-818c-472b-b12d-ff231c1b5745";
const CLIENT_SECRET: &str = "mvaiZkRsAsI1IBkY";
const AUTH_HOST: &str = "auth.api.sonyentertainmentnetwork.com";
const TOKEN_PATH: &str = "/2.0/oauth/token";
const REDIRECT_URI: &str = "https://remoteplay.dl.playstation.net/remoteplay/redirect";

pub fn login_url() -> String {
    format!(
        "https://{AUTH_HOST}/2.0/oauth/authorize?service_entity=urn:service-entity:psn&response_type=code&client_id={CLIENT_ID}&redirect_uri={REDIRECT_URI}&scope=psn:clientapp&request_locale=pl_PL&ui=pr&service_logo=ps&layout_type=popup&smcid=remoteplay&prompt=always&PlatformPrivacyWs1=minimal&"
    )
}

pub fn parse_redirect_code(input: &str) -> Result<String, String> {
    let input = input.trim();
    let query = input.split_once('?').map(|(_, q)| q).unwrap_or(input);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "code")
        .map(|(_, value)| value.to_string())
        .filter(|code| {
            !code.is_empty()
                && code
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.~%".contains(c))
        })
        .ok_or_else(|| {
            "The pasted address has no code parameter. Copy the whole address from the redirect page."
                .to_string()
        })
}

pub fn parse_account_id(input: &str) -> Result<[u8; 8], String> {
    let input = input.trim();
    if let Ok(bytes) = B64.decode(input)
        && let Ok(id) = <[u8; 8]>::try_from(bytes.as_slice())
    {
        return Ok(id);
    }
    if let Ok(number) = input.parse::<u64>() {
        return Ok(number.to_le_bytes());
    }
    Err("Account ID must be Base64 (for example AbCdEfGhIjK=) or a number".into())
}

pub fn account_id_base64(id: &[u8; 8]) -> String {
    B64.encode(id)
}

pub fn fetch_account_id(code: &str) -> Result<[u8; 8], String> {
    let auth = format!(
        "Authorization: Basic {}",
        B64.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    let body = format!("grant_type=authorization_code&code={code}&redirect_uri={REDIRECT_URI}&");
    let headers = format!("{auth}\r\nContent-Type: application/x-www-form-urlencoded");
    let (status, response) =
        https_request("POST", AUTH_HOST, TOKEN_PATH, &headers, body.as_bytes())?;
    if status != 200 {
        return Err(format!(
            "PSN rejected the login code (HTTP {status}). Sign in again and paste the new address."
        ));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&response).map_err(|_| "Invalid PSN response".to_string())?;
    let token = json["access_token"]
        .as_str()
        .ok_or("No token in the PSN response")?
        .to_string();

    let path = format!("{TOKEN_PATH}/{token}");
    let (status, response) = https_request("GET", AUTH_HOST, &path, &auth, &[])?;
    if status != 200 {
        return Err(format!("Cannot fetch account info (HTTP {status})"));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&response).map_err(|_| "Invalid PSN response".to_string())?;
    let user_id = match &json["user_id"] {
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
    .ok_or("The PSN response has no account ID")?;
    Ok(user_id.to_le_bytes())
}

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

fn https_request(
    method: &str,
    host: &str,
    path: &str,
    headers: &str,
    body: &[u8],
) -> Result<(u32, Vec<u8>), String> {
    let net_err = |what: &str| {
        format!(
            "Network error while {what}: {}",
            windows::core::Error::from_thread()
        )
    };
    unsafe {
        let session = Handle(WinHttpOpen(
            &HSTRING::from("PS5 LAN Capture"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(net_err("inicjalizacji"));
        }
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 15_000);
        let connect = Handle(WinHttpConnect(
            session.0,
            &HSTRING::from(host),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if connect.0.is_null() {
            return Err(net_err("connecting to PSN"));
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
            return Err(net_err("creating the request"));
        }
        let header_w: Vec<u16> = headers.encode_utf16().collect();
        WinHttpSendRequest(
            request.0,
            Some(&header_w),
            (!body.is_empty()).then_some(body.as_ptr() as *const _),
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .map_err(|e| format!("Network error: {e}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| format!("Network error: {e}"))?;

        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("Network error: {e}"))?;

        let mut response = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available)
                .map_err(|e| format!("Network error: {e}"))?;
            if available == 0 {
                break;
            }
            let start = response.len();
            response.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                response[start..].as_mut_ptr() as *mut _,
                available,
                &mut read,
            )
            .map_err(|e| format!("Network error: {e}"))?;
            response.truncate(start + read as usize);
            if response.len() > 1 << 20 {
                return Err("Server response too large".into());
            }
        }
        Ok((status, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_code_from_redirect_url() {
        let url = "https://remoteplay.dl.playstation.net/remoteplay/redirect?code=AbC123&cid=xyz";
        assert_eq!(parse_redirect_code(url).unwrap(), "AbC123");
    }

    #[test]
    fn rejects_url_without_code() {
        assert!(
            parse_redirect_code(
                "https://remoteplay.dl.playstation.net/remoteplay/redirect?error=denied"
            )
            .is_err()
        );
        assert!(parse_redirect_code("").is_err());
    }

    #[test]
    fn rejects_code_with_injection_characters() {
        assert!(
            parse_redirect_code("https://x/redirect?code=abc&def\r\nHost: evil")
                .is_ok_and(|c| c == "abc")
        );
        assert!(parse_redirect_code("https://x/redirect?code=a b").is_err());
    }

    #[test]
    fn parses_account_id_in_both_formats() {
        let id = 1234567890123456789u64;
        let b64 = account_id_base64(&id.to_le_bytes());
        assert_eq!(parse_account_id(&b64).unwrap(), id.to_le_bytes());
        assert_eq!(parse_account_id(&id.to_string()).unwrap(), id.to_le_bytes());
        assert!(parse_account_id("nope").is_err());
    }

    #[test]
    fn login_url_targets_sony() {
        assert!(login_url().starts_with("https://auth.api.sonyentertainmentnetwork.com/"));
    }
}
