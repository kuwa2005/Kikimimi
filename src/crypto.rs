//! 保存する秘密値（会員の API キー・FESS トークン）の暗号化と、乱数・ハッシュ。
//!
//! 鍵はデータフォルダの `secret.key`（32 バイト、利用者だけが読める権限）に置く。
//! DB には `v2:` ＋ base64(nonce ‖ 暗号文) の形で保存し、平文は残さない。
//! `v2` は「会員ID＋項目名」を付加データ（AAD）として暗号文に結び付ける。暗号文が漏れても、
//! 別の会員の行や別の項目に移すと復号に失敗し、使い回せない。
//! `v1`（付加データなし）は版 6 より前の値で、読むだけに使う。

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

static CIPHER: OnceLock<XChaCha20Poly1305> = OnceLock::new();

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).expect("OS の乱数");
    out
}

pub fn random_token() -> String {
    hex(&random_bytes::<32>())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_hex(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// 鍵ファイルを読み込む（無ければ作る）。起動時に一度だけ呼ぶ。
pub fn init(data_dir: &Path) -> Result<()> {
    let path = data_dir.join("secret.key");
    let key = match std::fs::read_to_string(&path) {
        Ok(text) => decode_hex(text.trim()).context("secret.key が壊れています")?,
        Err(_) => {
            let key = random_bytes::<32>().to_vec();
            write_private(&path, &hex(&key))?;
            key
        }
    };
    if key.len() != 32 {
        anyhow::bail!("secret.key の長さが不正です");
    }
    let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("32 バイトの鍵");
    let _ = CIPHER.set(cipher);
    Ok(())
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("{} を書けません", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

fn cipher() -> &'static XChaCha20Poly1305 {
    // テストなど init を呼ばない場面では、プロセス限りの鍵を使う。
    CIPHER.get_or_init(|| XChaCha20Poly1305::new_from_slice(&random_bytes::<32>()).expect("鍵"))
}

/// 会員の秘密値の結び付け先。会員IDと項目名（"api_key" など）。
/// 接頭辞 `gijiroku/` は保存済み暗号文の付加データなので、アプリ名を変えても維持する。
pub fn user_context(user_id: i64, field: &str) -> String {
    format!("gijiroku/user/{user_id}/{field}")
}

/// 会員IDと項目名に結び付けて暗号化する（`v2:`）。
pub fn seal_bound(plain: &str, context: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let nonce_bytes = random_bytes::<24>();
    let sealed = cipher()
        .encrypt(
            XNonce::from_slice(&nonce_bytes),
            Payload {
                msg: plain.as_bytes(),
                aad: context.as_bytes(),
            },
        )
        .expect("暗号化");
    let mut joined = nonce_bytes.to_vec();
    joined.extend(sealed);
    format!("v2:{}", base64::engine::general_purpose::STANDARD.encode(joined))
}

/// `seal_bound` の値を戻す。結び付け先が違えば失敗する。古い `v1:` も読む。
pub fn open_bound(stored: &str, context: &str) -> Result<String> {
    let Some(body) = stored.strip_prefix("v2:") else {
        return open(stored);
    };
    let joined = base64::engine::general_purpose::STANDARD
        .decode(body)
        .context("暗号文を読めません")?;
    if joined.len() < 24 {
        anyhow::bail!("暗号文が短すぎます");
    }
    let (nonce, sealed) = joined.split_at(24);
    let plain = cipher()
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: context.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("秘密値を復号できません（別の会員・項目の値か、secret.key が変わった可能性があります）"))?;
    Ok(String::from_utf8(plain)?)
}

pub fn seal(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let nonce_bytes = random_bytes::<24>();
    let nonce = XNonce::from_slice(&nonce_bytes);
    let sealed = cipher()
        .encrypt(nonce, plain.as_bytes())
        .expect("暗号化");
    let mut joined = nonce_bytes.to_vec();
    joined.extend(sealed);
    format!("v1:{}", base64::engine::general_purpose::STANDARD.encode(joined))
}

pub fn open(stored: &str) -> Result<String> {
    if stored.is_empty() {
        return Ok(String::new());
    }
    let body = stored
        .strip_prefix("v1:")
        .context("暗号化されていない値です")?;
    let joined = base64::engine::general_purpose::STANDARD
        .decode(body)
        .context("暗号文を読めません")?;
    if joined.len() < 24 {
        anyhow::bail!("暗号文が短すぎます");
    }
    let (nonce, sealed) = joined.split_at(24);
    let plain = cipher()
        .decrypt(XNonce::from_slice(nonce), sealed)
        .map_err(|_| anyhow::anyhow!("秘密値を復号できません（secret.key が変わった可能性があります）"))?;
    Ok(String::from_utf8(plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_value_cannot_move_to_another_user_or_field() {
        let sealed = seal_bound("sk-secret", &user_context(1, "api_key"));
        assert!(sealed.starts_with("v2:") && !sealed.contains("sk-secret"));
        assert_eq!(open_bound(&sealed, &user_context(1, "api_key")).unwrap(), "sk-secret");
        assert!(open_bound(&sealed, &user_context(2, "api_key")).is_err(), "別の会員では復号できない");
        assert!(open_bound(&sealed, &user_context(1, "openai_api_key")).is_err(), "別の項目では復号できない");
        let old = seal("sk-old");
        assert_eq!(open_bound(&old, &user_context(1, "api_key")).unwrap(), "sk-old", "v1 は読める");
    }

    #[test]
    fn sealed_values_round_trip_and_hide_the_plain_text() {
        let sealed = seal("AIza-secret");
        assert!(sealed.starts_with("v1:"));
        assert!(!sealed.contains("AIza"));
        assert_ne!(sealed, seal("AIza-secret"), "nonce は毎回変わる");
        assert_eq!(open(&sealed).unwrap(), "AIza-secret");
        assert_eq!(seal(""), "");
        assert_eq!(open("").unwrap(), "");
        assert!(open("plain").is_err());
    }
}
