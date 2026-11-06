//! PAM password check for `garde lock`. Blocking: call it from a worker thread.

use std::ffi::{CStr, CString};

use pam_client::{ConversationHandler, ErrorCode, Flag};
use zeroize::{Zeroize, Zeroizing};

pub const WRONG_PASSWORD: &str = "Wrong password";

/// A PAM message to show while (or after) checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub error: bool,
    pub text: String,
}

struct Conv<F: FnMut(Message)> {
    password: Option<Zeroizing<String>>,
    on_message: F,
    msgs: crate::greetd::Messages,
}

impl<F: FnMut(Message)> Conv<F> {
    fn record(&mut self, error: bool, msg: &CStr) {
        let text = msg.to_string_lossy().trim().to_string();
        if text.is_empty() {
            return;
        }
        log::debug!("PAM {}: {text}", if error { "error" } else { "info" });
        self.msgs.push(error, &text);
        (self.on_message)(Message { error, text });
    }
}

impl<F: FnMut(Message)> ConversationHandler for Conv<F> {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        // Only one field on screen: anything but the password can't be answered.
        Err(ErrorCode::CONV_ERR)
    }

    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        // The first hidden prompt gets the password; a second one (OTP, new password…) can't be
        // answered from a lock screen.
        let pw = self.password.take().ok_or(ErrorCode::CONV_ERR)?;
        let mut bytes = pw.as_bytes().to_vec();
        drop(pw);
        let c = CString::new(bytes.clone()).map_err(|e| {
            e.into_vec().zeroize();
            ErrorCode::CONV_ERR
        });
        bytes.zeroize();
        // The CString itself is handed to libpam through pam-client and dropped there unwiped.
        c
    }

    fn text_info(&mut self, msg: &CStr) {
        self.record(false, msg);
    }

    fn error_msg(&mut self, msg: &CStr) {
        self.record(true, msg);
    }
}

/// Check `password` for `user` with PAM `service`. `Err` carries the text to show: PAM's own
/// messages when it sent some (e.g. faillock's "account locked"), else a plain description.
pub fn check(
    service: &str,
    user: &str,
    password: Zeroizing<String>,
    on_message: impl FnMut(Message),
) -> Result<(), String> {
    let conv = Conv { password: Some(password), on_message, msgs: Default::default() };
    let mut ctx = pam_client::Context::new(service, Some(user), conv).map_err(|e| format!("PAM: {e}"))?;
    let res = ctx.authenticate(Flag::NONE).and_then(|()| match ctx.acct_mgmt(Flag::NONE) {
        // An expired password must not keep a running session locked forever.
        Err(e) if e.code() == ErrorCode::NEW_AUTHTOK_REQD => Ok(()),
        r => r,
    });
    match res {
        Ok(()) => {
            // Refresh credentials (Kerberos tickets etc.); failure here is not a reason to stay locked.
            if let Err(e) = ctx.reinitialize_credentials(Flag::NONE) {
                log::debug!("pam_setcred: {e}");
            }
            Ok(())
        }
        Err(e) => {
            log::debug!("PAM failed: {:?}", e.code());
            Err(ctx.conversation().msgs.explanation().unwrap_or_else(|| describe(e.code())))
        }
    }
}

pub fn describe(code: ErrorCode) -> String {
    match code {
        ErrorCode::AUTH_ERR | ErrorCode::CRED_ERR | ErrorCode::USER_UNKNOWN => WRONG_PASSWORD.into(),
        ErrorCode::MAXTRIES => "Too many failed attempts".into(),
        ErrorCode::PERM_DENIED => "Permission denied".into(),
        ErrorCode::ACCT_EXPIRED => "Account expired".into(),
        ErrorCode::AUTHINFO_UNAVAIL => "Authentication service unavailable".into(),
        ErrorCode::CONV_ERR => "Authentication needs more than a password".into(),
        other => format!("Authentication failed ({other:?})"),
    }
}
