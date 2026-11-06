//! 会員・セッション・プラン・利用量。
//!
//! - 新規登録のログインIDはメールアドレス。確認コードを照合してから会員を作る。プランはサーバが必ず Free を割り当てる。
//! - パスワードは Argon2id（個別の salt）で保存し、セッションはトークンのハッシュだけを保存する。
//! - 上限の判定は DB のトランザクション（IMMEDIATE）の中で行い、同時の操作でも超えない。

use std::fmt;

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::crypto;
use crate::db::now_stamp;

pub const SESSION_DAYS: i64 = 30;
pub const FREE: &str = "free";

#[cfg_attr(not(test), allow(dead_code))]
const RESERVED: &[&str] = &[
    "admin", "administrator", "root", "system", "gijiroku", "kikimimi", "api", "support", "help", "owner",
    "operator", "null", "undefined", "login", "logout", "register", "settings", "account", "free",
];

/// 登録に使うメールアドレスを正規化する。規則に合わなければエラーコードを返す。
pub fn normalize_email(raw: &str) -> Result<String, &'static str> {
    let email = raw.trim().to_ascii_lowercase();
    if email.len() > 254 || !email.is_ascii() {
        return Err("invalid_login_id");
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err("invalid_login_id");
    };
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
    {
        return Err("invalid_login_id");
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2
        || labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
    {
        return Err("invalid_login_id");
    }
    let local_ok = local
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "._+-".contains(ch));
    if !local_ok {
        return Err("invalid_login_id");
    }
    Ok(email)
}

/// 暫定の招待コード。前後の空白は無視し、大文字小文字は区別する。
pub fn invite_matches(raw: &str) -> bool {
    same_text(raw.trim(), "docomo2")
}

/// 以前からのログインIDを正規化する。規則に合わなければエラーコードを返す。
#[cfg_attr(not(test), allow(dead_code))]
pub fn normalize_login_id(raw: &str) -> Result<String, &'static str> {
    let id = raw.trim().to_ascii_lowercase();
    let ok_len = (3..=32).contains(&id.chars().count());
    let ok_chars = id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "_.-".contains(ch));
    let ok_edge = id
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric());
    if !ok_len || !ok_chars || !ok_edge || RESERVED.contains(&id.as_str()) {
        return Err("invalid_login_id");
    }
    Ok(id)
}

/// 8〜128 文字。文字の種類の組み合わせは問わないが、すべて同じ文字（`aaaaaaaa` など）は不可。
pub fn check_password(password: &str) -> Result<(), &'static str> {
    let count = password.chars().count();
    if !(8..=128).contains(&count) {
        return Err("weak_password");
    }
    let mut chars = password.chars();
    let first = chars.next();
    if chars.all(|ch| Some(ch) == first) {
        return Err("weak_password");
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&crypto::random_bytes::<16>())
        .map_err(|err| anyhow::anyhow!("salt: {err}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow::anyhow!("パスワードを保存できません: {err}"))
}

pub fn verify_password(stored: &str, password: &str) -> bool {
    PasswordHash::new(stored)
        .map(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
        .unwrap_or(false)
}

/// 存在しないIDでも同じだけ時間を掛けるための、照合用のダミー。
pub fn dummy_verify(password: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = DUMMY.get_or_init(|| hash_password("dummy-password-for-timing").unwrap_or_default());
    let _ = verify_password(hash, password);
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanRef {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub login_id: String,
    pub created_at: String,
    pub plan: PlanRef,
}

pub const SIGNUP_CODE_MINUTES: i64 = 10;
const SIGNUP_ATTEMPTS: i64 = 5;

/// 先頭が 0 のこともある、6桁の確認コード。
pub fn new_signup_code() -> String {
    loop {
        let n = u32::from_le_bytes(crypto::random_bytes::<4>());
        if n < 4_000_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

fn signup_code_hash(salt: &str, code: &str) -> String {
    crypto::sha256_hex(&format!("{salt}:{code}"))
}

fn same_text(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.bytes().zip(right.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// このメールアドレスの確認コードを置き換える。古いコードは使えなくなる。
pub fn store_signup_code(conn: &Connection, email: &str, code: &str, password_hash: &str) -> Result<()> {
    let salt = crypto::random_token();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(SIGNUP_CODE_MINUTES))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    conn.execute(
        "INSERT INTO signup_codes (email, salt, code_hash, password_hash, expires_at, attempts, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
         ON CONFLICT(email) DO UPDATE SET
           salt=excluded.salt,
           code_hash=excluded.code_hash,
           password_hash=excluded.password_hash,
           expires_at=excluded.expires_at,
           attempts=0,
           created_at=excluded.created_at",
        params![
            email,
            salt,
            signup_code_hash(&salt, code),
            password_hash,
            expires,
            now_stamp()
        ],
    )?;
    Ok(())
}

/// 確認コードが一致するかを見る。成功してもコードは消さない。
/// 不一致は試行回数を増やし、期限切れと回数超過はコードを消す。
pub fn match_signup_code(conn: &Connection, email: &str, code: &str) -> Result<Result<(), &'static str>> {
    let row = conn
        .query_row(
            "SELECT salt, code_hash, expires_at, attempts FROM signup_codes WHERE email=?1",
            [email],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((salt, code_hash, expires_at, attempts)) = row else {
        return Ok(Err("invalid_code"));
    };
    if expires_at.as_str() <= now_stamp().as_str() {
        delete_signup_code(conn, email)?;
        return Ok(Err("code_expired"));
    }
    if attempts >= SIGNUP_ATTEMPTS {
        delete_signup_code(conn, email)?;
        return Ok(Err("code_locked"));
    }
    if !same_text(&signup_code_hash(&salt, code), &code_hash) {
        let next = attempts + 1;
        if next >= SIGNUP_ATTEMPTS {
            delete_signup_code(conn, email)?;
            return Ok(Err("code_locked"));
        }
        conn.execute(
            "UPDATE signup_codes SET attempts=?1 WHERE email=?2",
            params![next, email],
        )?;
        return Ok(Err("invalid_code"));
    }
    Ok(Ok(()))
}

pub fn delete_signup_code(conn: &Connection, email: &str) -> Result<()> {
    conn.execute("DELETE FROM signup_codes WHERE email=?1", [email])?;
    Ok(())
}

/// パスワード再設定の確認コードを置き換える。
pub fn store_reset_code(conn: &Connection, email: &str, code: &str) -> Result<()> {
    let salt = crypto::random_token();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(SIGNUP_CODE_MINUTES))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    conn.execute(
        "INSERT INTO reset_codes (email, salt, code_hash, expires_at, attempts, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5)
         ON CONFLICT(email) DO UPDATE SET
           salt=excluded.salt,
           code_hash=excluded.code_hash,
           expires_at=excluded.expires_at,
           attempts=0,
           created_at=excluded.created_at",
        params![email, salt, signup_code_hash(&salt, code), expires, now_stamp()],
    )?;
    Ok(())
}

/// 再設定コードが一致したら Ok(Ok(())。不一致は試行回数を増やす。
pub fn match_reset_code(conn: &Connection, email: &str, code: &str) -> Result<Result<(), &'static str>> {
    let row = conn
        .query_row(
            "SELECT salt, code_hash, expires_at, attempts FROM reset_codes WHERE email=?1",
            [email],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?)),
        )
        .optional()?;
    let Some((salt, code_hash, expires_at, attempts)) = row else {
        return Ok(Err("invalid_code"));
    };
    if expires_at.as_str() <= now_stamp().as_str() {
        delete_reset_code(conn, email)?;
        return Ok(Err("code_expired"));
    }
    if attempts >= SIGNUP_ATTEMPTS {
        delete_reset_code(conn, email)?;
        return Ok(Err("code_locked"));
    }
    if !same_text(&signup_code_hash(&salt, code), &code_hash) {
        let next = attempts + 1;
        if next >= SIGNUP_ATTEMPTS {
            delete_reset_code(conn, email)?;
            return Ok(Err("code_locked"));
        }
        conn.execute("UPDATE reset_codes SET attempts=?1 WHERE email=?2", params![next, email])?;
        return Ok(Err("invalid_code"));
    }
    Ok(Ok(()))
}

pub fn delete_reset_code(conn: &Connection, email: &str) -> Result<()> {
    conn.execute("DELETE FROM reset_codes WHERE email=?1", [email])?;
    Ok(())
}

/// 新しい会員を作る。ログインIDが使われていれば Ok(None)。
pub fn create_user(conn: &Connection, login_id: &str, password_hash: &str) -> Result<Option<i64>> {
    let tx = conn.unchecked_transaction()?;
    let inserted = tx.execute(
        "INSERT INTO users (login_id, password_hash, status, created_at) VALUES (?1, ?2, 'active', ?3)
         ON CONFLICT(login_id) DO NOTHING",
        params![login_id, password_hash, now_stamp()],
    )?;
    if inserted == 0 {
        return Ok(None);
    }
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO plan_assignments (user_id, plan_code, started_at, source) VALUES (?1, ?2, ?3, 'register')",
        params![id, FREE, now_stamp()],
    )?;
    tx.execute(
        "INSERT INTO user_settings (user_id, model, summary_prompt) VALUES (?1, ?2, ?3)",
        params![id, crate::db::DEFAULT_MODEL, crate::db::DEFAULT_SUMMARY_PROMPT],
    )?;
    tx.commit()?;
    Ok(Some(id))
}

pub struct Credentials {
    pub id: i64,
    pub password_hash: String,
    pub active: bool,
}

pub fn credentials(conn: &Connection, login_id: &str) -> Result<Option<Credentials>> {
    Ok(conn
        .query_row(
            "SELECT id, password_hash, status FROM users WHERE login_id=?1",
            [login_id],
            |row| {
                Ok(Credentials {
                    id: row.get(0)?,
                    password_hash: row.get(1)?,
                    active: row.get::<_, String>(2)? == "active",
                })
            },
        )
        .optional()?)
}

pub fn user(conn: &Connection, id: i64) -> Result<Option<User>> {
    let row = conn
        .query_row(
            "SELECT id, login_id, created_at FROM users WHERE id=?1",
            [id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        )
        .optional()?;
    let Some((id, login_id, created_at)) = row else {
        return Ok(None);
    };
    let plan = plan_for_user(conn, id)?;
    Ok(Some(User {
        id,
        login_id,
        created_at,
        plan: PlanRef {
            code: plan.code,
            name: plan.name,
        },
    }))
}

pub fn user_id_by_login(conn: &Connection, login_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM users WHERE login_id=?1", [login_id], |row| row.get(0))
        .optional()?)
}

pub fn list_users(conn: &Connection) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare("SELECT id, login_id, created_at FROM users ORDER BY id")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_password(conn: &Connection, user_id: i64, password_hash: &str) -> Result<()> {
    conn.execute(
        "UPDATE users SET password_hash=?1 WHERE id=?2",
        params![password_hash, user_id],
    )?;
    Ok(())
}

// ---- セッション ----

pub struct Session {
    pub id: i64,
    pub user_id: i64,
    pub login_id: String,
    pub csrf: String,
}

/// 新しいセッションを作り、Cookie に入れるトークンと CSRF トークンを返す。
pub fn create_session(conn: &Connection, user_id: i64) -> Result<(String, String)> {
    let token = crypto::random_token();
    let csrf = crypto::random_token();
    let expires = chrono::Utc::now() + chrono::Duration::days(SESSION_DAYS);
    conn.execute(
        "INSERT INTO sessions (user_id, token_hash, csrf, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            user_id,
            crypto::sha256_hex(&token),
            csrf,
            now_stamp(),
            expires.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        ],
    )?;
    Ok((token, csrf))
}

pub fn session(conn: &Connection, token: &str) -> Result<Option<Session>> {
    Ok(conn
        .query_row(
            "SELECT s.id, s.user_id, u.login_id, s.csrf FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash=?1 AND s.revoked_at IS NULL AND s.expires_at > ?2 AND u.status='active'",
            params![crypto::sha256_hex(token), now_stamp()],
            |row| {
                Ok(Session {
                    id: row.get(0)?,
                    user_id: row.get(1)?,
                    login_id: row.get(2)?,
                    csrf: row.get(3)?,
                })
            },
        )
        .optional()?)
}

pub fn revoke_session(conn: &Connection, session_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
        params![now_stamp(), session_id],
    )?;
    Ok(())
}

/// パスワードを変えたときなどに、他の端末のセッションを失効させる。
pub fn revoke_other_sessions(conn: &Connection, user_id: i64, keep: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET revoked_at=?1 WHERE user_id=?2 AND revoked_at IS NULL AND id IS NOT ?3",
        params![now_stamp(), user_id, keep],
    )?;
    Ok(())
}

pub fn purge_expired_sessions(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM sessions WHERE expires_at < ?1 OR revoked_at IS NOT NULL",
        [now_stamp()],
    )?;
    Ok(())
}

// ---- プランと上限 ----

/// 上限。None は無制限。プランの定義（JSON）に項目が無いときは 0（使えない）として扱い、
/// 未設定が意図せず無制限にならないようにする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub daily_meetings: Option<i64>,
    pub meeting_minutes: Option<i64>,
    pub storage_bytes: Option<i64>,
    pub concurrent_recordings: Option<i64>,
    /// 機能: リアルタイム文字起こし（Chrome の速報字幕, #104）。上位プランとの差別化に使う。
    pub realtime_captions: bool,
}

impl Limits {
    fn to_json(&self) -> Value {
        json!({
            "daily_meetings": self.daily_meetings,
            "meeting_minutes": self.meeting_minutes,
            "storage_bytes": self.storage_bytes,
            "concurrent_recordings": self.concurrent_recordings,
            "realtime_captions": self.realtime_captions,
        })
    }

    fn from_json(text: &str) -> Self {
        let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let read = |key: &str| match value.get(key) {
            Some(Value::Null) => None,
            Some(number) => Some(number.as_i64().unwrap_or(0)),
            None => {
                tracing::warn!("プランに上限 {key} の定義がありません。0 として扱います");
                Some(0)
            }
        };
        Limits {
            daily_meetings: read("daily_meetings"),
            meeting_minutes: read("meeting_minutes"),
            storage_bytes: read("storage_bytes"),
            concurrent_recordings: read("concurrent_recordings"),
            // 機能の項目が無い古い定義は「使えない」として扱う。
            realtime_captions: value.get("realtime_captions").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

/// Free プランの上限。運用者が環境変数で変えられる。`unlimited` は無制限。
/// 既定値は設計・動作確認のための仮の値で、料金仕様として承認されたものではない。
pub fn free_limits_from_env() -> Limits {
    fn read(name: &str, default: i64, scale: i64) -> Option<i64> {
        match std::env::var(name) {
            Ok(value) if value.trim().eq_ignore_ascii_case("unlimited") => None,
            Ok(value) => match value.trim().parse::<i64>() {
                Ok(number) if number >= 0 => Some(number * scale),
                _ => {
                    tracing::warn!("{name}={value} は不正です。既定値 {default} を使います");
                    Some(default * scale)
                }
            },
            Err(_) => Some(default * scale),
        }
    }
    Limits {
        daily_meetings: read("KIKIMIMI_FREE_DAILY_MEETINGS", 30, 1),
        // 暫定で 10 時間（#102）。有料プランを始めるときに見直す。
        meeting_minutes: read("KIKIMIMI_FREE_MEETING_MINUTES", 600, 1),
        storage_bytes: read("KIKIMIMI_FREE_STORAGE_MB", 1024, 1024 * 1024),
        concurrent_recordings: read("KIKIMIMI_FREE_CONCURRENT_RECORDINGS", 1, 1),
        // リアルタイム文字起こし。いまは Free でも使える。上位プランを始めたら 0 にする（#104）。
        realtime_captions: std::env::var("KIKIMIMI_FREE_REALTIME_CAPTIONS")
            .map(|value| !matches!(value.trim(), "0" | "false" | "off" | "no"))
            .unwrap_or(true),
    }
}

pub fn upsert_free_plan(conn: &Connection, limits: &Limits) -> Result<()> {
    conn.execute(
        "INSERT INTO plans (code, name, registrable, public, limits, version) VALUES (?1, 'Free', 1, 1, ?2, 1)
         ON CONFLICT(code) DO UPDATE SET limits=excluded.limits,
           version = plans.version + (plans.limits != excluded.limits)",
        params![FREE, limits.to_json().to_string()],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanInfo {
    pub code: String,
    pub name: String,
    pub registrable: bool,
    #[serde(skip)]
    pub limits: Limits,
    /// このプランで使える機能（画面で出し分ける）。
    pub features: Value,
}

/// 会員の有効なプラン。割り当てが無ければ Free。
pub fn plan_for_user(conn: &Connection, user_id: i64) -> Result<PlanInfo> {
    let row = conn
        .query_row(
            "SELECT p.code, p.name, p.registrable, p.limits FROM plan_assignments a JOIN plans p ON p.code = a.plan_code
             WHERE a.user_id=?1 AND a.ended_at IS NULL ORDER BY a.id DESC LIMIT 1",
            [user_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?)),
        )
        .optional()?;
    let (code, name, registrable, limits) = match row {
        Some(row) => row,
        None => conn
            .query_row(
                "SELECT code, name, registrable, limits FROM plans WHERE code=?1",
                [FREE],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .context("Free プランが定義されていません")?,
    };
    Ok(PlanInfo {
        code,
        name,
        registrable: registrable != 0,
        features: json!({"realtime_captions": Limits::from_json(&limits).realtime_captions}),
        limits: Limits::from_json(&limits),
    })
}

// ---- 利用量 ----

#[derive(Debug, Clone, Serialize)]
pub struct UsageItem {
    pub item: &'static str,
    pub label: &'static str,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    pub unit: &'static str,
    pub period_end: Option<String>,
}

/// 上限に当たったことを表すエラー。routes で 403 limit_exceeded に変える。
#[derive(Debug, Clone)]
pub struct LimitExceeded(pub UsageItem);

impl fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let item = &self.0;
        let limit = item
            .limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "無制限".into());
        write!(f, "{}の上限（{} {}）に達しました", item.label, limit, item.unit)
    }
}

impl std::error::Error for LimitExceeded {}

/// 集計日の時差。`KIKIMIMI_USAGE_UTC_OFFSET`（既定 +09:00）。
fn usage_offset() -> chrono::FixedOffset {
    std::env::var("KIKIMIMI_USAGE_UTC_OFFSET")
        .ok()
        .and_then(|value| value.parse::<chrono::FixedOffset>().ok())
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(9 * 3600).expect("+09:00"))
}

/// いまの集計日（`2026-11-02`、`KIKIMIMI_USAGE_UTC_OFFSET` の日付）と、その終わり（翌日 0 時の UTC 時刻）。
pub fn current_period() -> (String, String) {
    use chrono::{Datelike, TimeZone};
    let offset = usage_offset();
    let now = chrono::Utc::now().with_timezone(&offset);
    let (year, month, day) = (now.year(), now.month(), now.day());
    let start = offset
        .with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .expect("当日の始まり");
    let end = (start + chrono::Duration::days(1)).with_timezone(&chrono::Utc);
    (
        format!("{year:04}-{month:02}-{day:02}"),
        end.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

fn count_meeting_starts(conn: &Connection, user_id: i64, period: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM usage_events WHERE user_id=?1 AND kind='meeting_start' AND period=?2",
        params![user_id, period],
        |row| row.get(0),
    )?)
}

fn live_meetings(conn: &Connection, user_id: i64, except: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM meetings WHERE user_id=?1 AND status='live' AND finish_requested=0 AND id<>?2",
        params![user_id, except],
        |row| row.get(0),
    )?)
}

pub fn storage_used(conn: &Connection, user_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(j.bytes), 0) FROM audio_jobs j JOIN meetings m ON m.id = j.meeting_id WHERE m.user_id=?1",
        [user_id],
        |row| row.get(0),
    )?)
}

/// 会議で受け付けた録音の長さ（ミリ秒）。区間の音声から測った値で、クライアントの時計は使わない。
pub fn meeting_recorded_ms(conn: &Connection, meeting_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(duration_ms), 0) FROM audio_jobs WHERE meeting_id=?1",
        [meeting_id],
        |row| row.get(0),
    )?)
}

pub fn usage(conn: &Connection, user_id: i64, limits: &Limits) -> Result<Vec<UsageItem>> {
    let (period, period_end) = current_period();
    Ok(vec![
        UsageItem {
            item: "daily_meetings",
            label: "今日開始した会議",
            used: Some(count_meeting_starts(conn, user_id, &period)?),
            limit: limits.daily_meetings,
            unit: "件",
            period_end: Some(period_end),
        },
        UsageItem {
            item: "meeting_minutes",
            label: "1 会議の録音時間",
            used: None,
            limit: limits.meeting_minutes,
            unit: "分",
            period_end: None,
        },
        UsageItem {
            item: "storage_bytes",
            label: "録音の保存容量",
            used: Some(storage_used(conn, user_id)?),
            limit: limits.storage_bytes,
            unit: "バイト",
            period_end: None,
        },
        UsageItem {
            item: "concurrent_recordings",
            label: "同時に録音できる会議",
            used: Some(live_meetings(conn, user_id, 0)?),
            limit: limits.concurrent_recordings,
            unit: "件",
            period_end: None,
        },
    ])
}

fn exceeded(item: &'static str, label: &'static str, used: i64, limit: i64, unit: &'static str, period_end: Option<String>) -> anyhow::Error {
    LimitExceeded(UsageItem {
        item,
        label,
        used: Some(used),
        limit: Some(limit),
        unit,
        period_end,
    })
    .into()
}

/// 会議の開始（下書き → 録音中）。同時録音数と今日の会議数を確かめてから計上する。
/// 開始は下書きからの一度しか起こらず（呼び出し側が状態を確かめる）、IMMEDIATE のトランザクションで
/// 直列になるので、二重には数えない。会議 ID は削除後に使い回されることがあるため、記録のキーには使わない。
pub fn gate_start(conn: &Connection, user_id: i64, meeting_id: i64, limits: &Limits) -> Result<()> {
    if let Some(limit) = limits.concurrent_recordings {
        let live = live_meetings(conn, user_id, meeting_id)?;
        if live >= limit {
            return Err(exceeded("concurrent_recordings", "同時に録音できる会議", live, limit, "件", None));
        }
    }
    let (period, period_end) = current_period();
    if let Some(limit) = limits.daily_meetings {
        let used = count_meeting_starts(conn, user_id, &period)?;
        if used >= limit {
            return Err(exceeded("daily_meetings", "今日開始した会議", used, limit, "件", Some(period_end)));
        }
    }
    conn.execute(
        "INSERT INTO usage_events (user_id, kind, key, amount, period, created_at) VALUES (?1, 'meeting_start', ?2, 1, ?3, ?4)",
        params![
            user_id,
            format!("meeting_start:{meeting_id}:{}", crypto::random_token()),
            period,
            now_stamp()
        ],
    )?;
    Ok(())
}

/// 一時停止からの再開。同時録音数だけを確かめる。
pub fn gate_resume(conn: &Connection, user_id: i64, meeting_id: i64, limits: &Limits) -> Result<()> {
    if let Some(limit) = limits.concurrent_recordings {
        let live = live_meetings(conn, user_id, meeting_id)?;
        if live >= limit {
            return Err(exceeded("concurrent_recordings", "同時に録音できる会議", live, limit, "件", None));
        }
    }
    Ok(())
}

/// 録音区間の受け付け。1 会議の録音時間と保存容量を確かめる。
pub fn gate_audio(
    conn: &Connection,
    user_id: i64,
    meeting_id: i64,
    duration_ms: i64,
    bytes: i64,
    limits: &Limits,
) -> Result<()> {
    if let Some(limit) = limits.meeting_minutes {
        let used = meeting_recorded_ms(conn, meeting_id)?;
        if used + duration_ms > limit * 60_000 {
            return Err(exceeded("meeting_minutes", "1 会議の録音時間", used / 60_000, limit, "分", None));
        }
    }
    if let Some(limit) = limits.storage_bytes {
        let used = storage_used(conn, user_id)?;
        if used + bytes > limit {
            return Err(exceeded("storage_bytes", "録音の保存容量", used, limit, "バイト", None));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_ids_are_normalized_and_checked() {
        assert_eq!(normalize_login_id("  Tanaka.T "), Ok("tanaka.t".into()));
        assert_eq!(normalize_login_id("ab"), Err("invalid_login_id"));
        assert_eq!(normalize_login_id("admin"), Err("invalid_login_id"));
        assert_eq!(normalize_login_id("_x1"), Err("invalid_login_id"));
        assert_eq!(normalize_login_id("田中"), Err("invalid_login_id"));
        assert_eq!(normalize_email("  Ada@Example.com "), Ok("ada@example.com".into()));
        assert_eq!(normalize_email("a.b+c@example.co.jp"), Ok("a.b+c@example.co.jp".into()));
        assert!(normalize_email("ada").is_err());
        assert!(normalize_email("ada@localhost").is_err());
        assert_eq!(normalize_email("a@b.c"), Ok("a@b.c".into()));
        assert!(invite_matches(" docomo2 "));
        assert!(!invite_matches("Docomo2"));
        assert!(!invite_matches("docomo"));
        assert!(check_password("short").is_err());
        assert!(check_password("1234567").is_err(), "7 文字は短い");
        assert!(check_password("12345678").is_ok(), "数字だけでも 8 文字ならよい");
        assert!(check_password("abcdefgh").is_ok());
        assert!(check_password("aaaaaaaa").is_err(), "すべて同じ文字は不可");
        assert!(check_password("ああああああああああ").is_err());
        assert!(check_password(&"x".repeat(129)).is_err());
    }

    #[test]
    fn passwords_are_hashed_with_argon2id() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "correct horse battery"));
        assert!(!verify_password(&hash, "wrong password!!"));
        assert_ne!(hash, hash_password("correct horse battery").unwrap(), "salt は毎回変わる");
    }

    #[test]
    fn missing_limit_is_not_unlimited() {
        let limits = Limits::from_json(r#"{"daily_meetings": null, "meeting_minutes": 60}"#);
        assert_eq!(limits.daily_meetings, None);
        assert_eq!(limits.meeting_minutes, Some(60));
        assert_eq!(limits.storage_bytes, Some(0));
    }

    #[test]
    fn period_end_is_the_next_day() {
        let (period, end) = current_period();
        assert_eq!(period.len(), 10);
        assert!(end.ends_with("T15:00:00Z") || std::env::var("KIKIMIMI_USAGE_UTC_OFFSET").is_ok());
    }
}
