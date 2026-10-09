//! Account identity captured when an action is queued, checked before each request.

use crate::config::Cookies;
use anyhow::{Result, bail};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// A guest response is an authentication signal only when the server states it
/// explicitly. Missing tracking metadata is not evidence of an expired login.
pub(crate) fn logged_in(json: &serde_json::Value) -> Option<bool> {
    json.pointer("/responseContext/serviceTrackingParams")?
        .as_array()?
        .iter()
        .filter_map(|service| service["params"].as_array())
        .flatten()
        .find_map(|param| {
            if param["key"] != "logged_in" {
                return None;
            }
            match param["value"].as_str()? {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            }
        })
}

pub struct Session {
    cookies: Cookies,
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl Session {
    pub fn capture(generation: Arc<AtomicU64>) -> Result<Self> {
        let expected = generation.load(Ordering::SeqCst);
        let cookies = Cookies::available()?
            .ok_or_else(|| anyhow::anyhow!("Sign in to YouTube Music first."))?;
        Ok(Self {
            cookies,
            generation,
            expected,
        })
    }

    #[cfg(test)]
    pub fn current() -> Result<Self> {
        Self::capture(Arc::new(AtomicU64::new(0)))
    }

    pub fn cookies(&self) -> Result<&Cookies> {
        let current = Cookies::available()?;
        if !same_session(
            self.expected,
            self.generation.load(Ordering::SeqCst),
            self.cookies.header(),
            current.as_ref().map(Cookies::header),
        ) {
            bail!("Your account changed while this action was waiting. Try again.");
        }
        Ok(&self.cookies)
    }
}

fn same_session(expected: u64, actual: u64, header: &str, current: Option<&str>) -> bool {
    expected == actual && current == Some(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_server_login_flags_mark_an_expired_session() {
        let response = |flag| {
            serde_json::json!({"responseContext":{"serviceTrackingParams":[
            {"params":[{"key":"logged_in","value":flag}]}]}})
        };
        assert_eq!(logged_in(&response("1")), Some(true));
        assert_eq!(logged_in(&response("0")), Some(false));
        assert_eq!(logged_in(&response("unknown")), None);
        assert_eq!(logged_in(&serde_json::json!({"contents":{}})), None);
    }
    #[test]
    fn a_queued_action_cannot_follow_logout_or_an_account_change() {
        assert!(same_session(1, 1, "account-a", Some("account-a")));
        assert!(!same_session(1, 2, "account-a", Some("account-a")));
        assert!(!same_session(1, 1, "account-a", Some("account-b")));
        assert!(!same_session(1, 1, "account-a", None));
    }
}
