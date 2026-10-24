use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::http;

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
        "https://{AUTH_HOST}/2.0/oauth/authorize?service_entity=urn:service-entity:psn&response_type=code&client_id={CLIENT_ID}&redirect_uri={REDIRECT_URI}&scope=psn:clientapp&request_locale=en_US&ui=pr&service_logo=ps&layout_type=popup&smcid=remoteplay&prompt=always&PlatformPrivacyWs1=minimal&"
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
    let (status, response) = http::fetch(
        "POST",
        &format!("https://{AUTH_HOST}{TOKEN_PATH}"),
        &headers,
        body.as_bytes(),
        1 << 20,
    )?;
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

    let url = format!("https://{AUTH_HOST}{TOKEN_PATH}/{token}");
    let (status, response) = http::fetch("GET", &url, &auth, &[], 1 << 20)?;
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
