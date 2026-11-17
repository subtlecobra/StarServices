//! Outgoing mail, for email verification and password reset. No sender exists yet, so the only one is `off`: the
//! portal then offers neither, and an admin resets passwords (sc-admin-web, `SetAccountState`). A real sender (SMTP, or
//! a provider's API) is another `Mailer` chosen by `SCBACKEND_MAIL`, and the flows that need it check `enabled`.

pub trait Mailer: Send + Sync {
    fn enabled(&self) -> bool;
    /// Sends one message. Only called when `enabled`; with no sender yet, no flow calls it.
    #[allow(dead_code)]
    fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String>;
}

pub struct Off;

impl Mailer for Off {
    fn enabled(&self) -> bool {
        false
    }

    fn send(&self, _to: &str, _subject: &str, _body: &str) -> Result<(), String> {
        Err("no mail sender is configured".to_string())
    }
}

/// The sender named by `SCBACKEND_MAIL`.
pub fn from_config(name: &str) -> Result<Box<dyn Mailer>, String> {
    match name {
        "" | "off" => Ok(Box::new(Off)),
        other => Err(format!("SCBACKEND_MAIL: unknown mail sender '{other}' (only 'off' exists)")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mail_is_off_by_default_and_unknown_senders_refused() {
        let off = super::from_config("off").unwrap();
        assert!(!off.enabled());
        assert!(off.send("a@b.c", "s", "b").is_err());
        assert!(super::from_config("smtp").is_err());
    }
}
