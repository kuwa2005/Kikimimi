//! 登録確認コードのメール。
//!
//! SMTP は環境変数で指定する。未設定のとき、`KIKIMIMI_MAIL_DIR` があればそこへ本文を書き、
//! どちらも無ければ送れない。確認コードはログに出さない。

#[cfg(not(test))]
use std::path::PathBuf;

#[cfg(not(test))]
use lettre::message::header::ContentType;
#[cfg(not(test))]
use lettre::message::Mailbox;
#[cfg(not(test))]
use lettre::transport::smtp::authentication::Credentials;
#[cfg(not(test))]
use lettre::transport::smtp::client::{Tls, TlsParameters};
#[cfg(not(test))]
use lettre::{Message, SmtpTransport, Transport};

#[cfg_attr(test, allow(dead_code))]
pub enum MailError {
    Unconfigured,
    Failed(String),
}

/// 件名と本文。説明は一文だけにし、URL や案内リンクは入れない。
pub fn signup_message(code: &str) -> (String, String) {
    (
        "会員登録の確認コード".into(),
        format!("会員登録の確認コードです。登録画面に、10分以内に入力してください。\n\n{code}\n"),
    )
}

pub fn reset_message(code: &str) -> (String, String) {
    (
        "パスワード再設定の確認コード".into(),
        format!("パスワード再設定の確認コードです。再設定画面に、10分以内に入力してください。\n\n{code}\n"),
    )
}

pub fn send_signup_code(to: &str, code: &str) -> Result<(), MailError> {
    let (subject, body) = signup_message(code);
    #[cfg(test)]
    {
        let _ = (to, &subject, &body);
        return Ok(());
    }
    #[cfg(not(test))]
    {
        if smtp_host().is_none() {
            if let Some(dir) = mail_dir() {
                return write_dev_mail(&dir, to, code);
            }
            return Err(MailError::Unconfigured);
        }
        send_smtp(to, &subject, &body)
    }
}

pub fn send_reset_code(to: &str, code: &str) -> Result<(), MailError> {
    let (subject, body) = reset_message(code);
    #[cfg(test)]
    {
        let _ = (to, &subject, &body);
        return Ok(());
    }
    #[cfg(not(test))]
    {
        if smtp_host().is_none() {
            if let Some(dir) = mail_dir() {
                return write_dev_mail(&dir, to, code);
            }
            return Err(MailError::Unconfigured);
        }
        send_smtp(to, &subject, &body)
    }
}

#[cfg(not(test))]
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

#[cfg(not(test))]
fn smtp_host() -> Option<String> {
    env_nonempty("KIKIMIMI_SMTP_HOST")
}

#[cfg(not(test))]
fn mail_dir() -> Option<PathBuf> {
    env_nonempty("KIKIMIMI_MAIL_DIR").map(PathBuf::from)
}

#[cfg(not(test))]
fn write_dev_mail(dir: &std::path::Path, to: &str, code: &str) -> Result<(), MailError> {
    std::fs::create_dir_all(dir).map_err(|err| MailError::Failed(err.to_string()))?;
    let name = format!("{}-{}.txt", chrono::Utc::now().timestamp_millis(), crypto_tag(to));
    let path = dir.join(name);
    std::fs::write(&path, format!("to: {to}\ncode: {code}\n")).map_err(|err| MailError::Failed(err.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(not(test))]
fn crypto_tag(to: &str) -> String {
    crate::crypto::sha256_hex(to).chars().take(12).collect()
}

#[cfg(not(test))]
fn send_smtp(to: &str, subject: &str, body: &str) -> Result<(), MailError> {
    let host = smtp_host().ok_or(MailError::Unconfigured)?;
    let from_raw = env_nonempty("KIKIMIMI_SMTP_FROM").ok_or(MailError::Unconfigured)?;
    let from: Mailbox = from_raw.parse().map_err(|_| MailError::Unconfigured)?;
    let to_box: Mailbox = to.parse().map_err(|_| MailError::Failed("宛先のメールアドレスを解釈できません".into()))?;
    let message = Message::builder()
        .from(from)
        .to(to_box)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|err| MailError::Failed(err.to_string()))?;
    let port = env_nonempty("KIKIMIMI_SMTP_PORT")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(587);
    let mode = env_nonempty("KIKIMIMI_SMTP_TLS").unwrap_or_else(|| {
        if port == 465 {
            "tls".into()
        } else {
            "starttls".into()
        }
    });
    let mut builder = match mode.as_str() {
        "none" => SmtpTransport::builder_dangerous(&host).port(port),
        "tls" => {
            let tls = TlsParameters::new(host.clone()).map_err(|err| MailError::Failed(err.to_string()))?;
            SmtpTransport::relay(&host)
                .map_err(|err| MailError::Failed(err.to_string()))?
                .port(port)
                .tls(Tls::Wrapper(tls))
        }
        _ => SmtpTransport::starttls_relay(&host)
            .map_err(|err| MailError::Failed(err.to_string()))?
            .port(port),
    };
    if let Some(user) = env_nonempty("KIKIMIMI_SMTP_USER") {
        let password = std::env::var("KIKIMIMI_SMTP_PASSWORD").unwrap_or_default();
        builder = builder.credentials(Credentials::new(user, password));
    }
    builder
        .build()
        .send(&message)
        .map(|_| ())
        .map_err(|err| MailError::Failed(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::signup_message;

    #[test]
    fn signup_mail_is_only_a_short_note_and_the_code() {
        let (subject, body) = signup_message("123456");
        assert_eq!(subject, "会員登録の確認コード");
        assert_eq!(
            body,
            "会員登録の確認コードです。登録画面に、10分以内に入力してください。\n\n123456\n"
        );
        let text = format!("{subject}\n{body}").to_ascii_lowercase();
        assert!(!text.contains("http"));
        assert!(!text.contains("www."));
        assert!(!text.contains("://"));
        let (reset_subject, reset_body) = super::reset_message("654321");
        assert_eq!(reset_subject, "パスワード再設定の確認コード");
        assert_eq!(
            reset_body,
            "パスワード再設定の確認コードです。再設定画面に、10分以内に入力してください。\n\n654321\n"
        );
        let reset = format!("{reset_subject}\n{reset_body}").to_ascii_lowercase();
        assert!(!reset.contains("http"));
        assert!(!reset.contains("www."));
        assert!(!reset.contains("://"));
    }
}
