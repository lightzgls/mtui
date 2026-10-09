//! Actionable provider errors without exposing URLs or credentials.

use serde_json::Value;

pub(super) fn message(status: u16, error: Option<&Value>) -> String {
    let code = error.and_then(|error| error["code"].as_u64())
        .and_then(|code| u16::try_from(code).ok()).unwrap_or(status);
    let guidance = match code {
        401 => "Your YouTube Music session expired. Sign in again.",
        403 => "YouTube Music denied permission for this action. Check that this account can edit the playlist.",
        429 => "YouTube Music is receiving too many requests. Wait a moment and try again.",
        500..=599 => "YouTube Music is temporarily unavailable. Try again shortly.",
        _ => "YouTube Music rejected this request.",
    };
    let detail = error.and_then(|error| error["message"].as_str())
        .map(crate::diagnostics::safe_message)
        .filter(|message| !message.is_empty() && !message.starts_with("[sensitive"))
        .map(|message| message.chars().take(160).collect::<String>());
    match detail {
        Some(detail) => format!("{guidance} HTTP {code}: {detail}"),
        None => format!("{guidance} HTTP {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn permissions_rate_limits_and_server_errors_do_not_request_login() {
        for status in [400, 403, 429, 500] {
            assert!(!message(status, None).contains("Sign in"));
        }
        assert!(message(401, None).contains("Sign in"));
        assert!(message(200, Some(&json!({"code":429}))).contains("Wait"));
    }
    #[test]
    fn provider_details_are_redacted_and_bounded() {
        assert!(!message(403, Some(&json!({"message":"cookie=private"}))).contains("private"));
        assert!(!message(400, Some(&json!({"message":"at https://private/path"}))).contains("private"));
        assert!(message(400, Some(&json!({"message":"x".repeat(10000)}))).len() < 250);
    }
}
