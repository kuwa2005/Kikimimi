use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

pub const DEFAULT_MODEL: &str = "gemini-3.8-flash";

/// 一時エラーで同じ区間を Gemini に送る回数の上限。打ち切りと再開で同じ値を使う。
pub const MAX_ATTEMPTS: i64 = 5;

const LATEST_VERSION: i64 = 14;

/// 版 7 より前の既定の要約プロンプト。変えていない会員は、移行で新しい既定に置き換える。
const OLD_SUMMARY_PROMPT_V6: &str = "\
あなたは優秀な議事録作成AIです。
提供された書き起こし全文、質問への回答候補、ピン留めされた社内ナレッジを分析し、
次の形式の Markdown で議事録を作成してください。

## 会議の概要
## 議論の背景・内容
## 決定事項
## アクションアイテム

箇条書きで簡潔にまとめてください。
ナレッジを根拠に使った箇所は、どの資料に基づくかを一言添えてください。";

/// 版 9 より前（版 7〜8）の既定の要約プロンプト。変えていない会員は、移行で新しい既定に置き換える。
const OLD_SUMMARY_PROMPT_V8: &str = "\
あなたは優秀な議事録作成AIです。
提供された書き起こし全文、聞くことへの回答候補、会議中のメモ、ピン留めされた社内ナレッジを分析し、
次の形式の Markdown で議事録を作成してください。

## 会議の概要
## 議論の背景・内容
## 決定事項
## アクションアイテム
## 聞けたこと・聞けなかったこと
## 次回確認すること

箇条書きで簡潔にまとめてください。「ここ重要」と印の付いた発言は必ず反映してください。
聞けなかったことは、次回どう聞くとよいかを一言添えてください。
ナレッジを根拠に使った箇所は、どの資料に基づくかを一言添えてください。";

pub const DEFAULT_SUMMARY_PROMPT: &str = "\
あなたは優秀な議事録作成AIです。
提供された書き起こし全文、アジェンダ（議題）ごとの回答・結論、会議中のメモ、ピン留めされた資料を分析し、
次の形式の Markdown で議事録を作成してください。

## 会議の概要
## 議論の背景・内容
## 決定事項
## アクションアイテム
## 聞けたこと・聞けなかったこと
## 次回確認すること

箇条書きで簡潔にまとめてください。「ここ重要」と印の付いた発言は必ず反映してください。
聞けなかったことは、次回どう聞くとよいかを一言添えてください。
ナレッジを根拠に使った箇所は、どの資料に基づくかを一言添えてください。";

pub fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    // 消した・書き換えた値（キーの平文など）が空きページに残らないよう、0 で上書きさせる。
    conn.pragma_update(None, "secure_delete", "ON")?;
    migrate_with_backup(&conn, Some(path))?;
    crate::accounts::upsert_free_plan(&conn, &crate::accounts::free_limits_from_env())?;
    Ok(conn)
}

/// 会員制（版 4）への移行は既存データの持ち主を変えるため、移行の前に DB をまるごと残す。
fn migrate_with_backup(conn: &Connection, path: Option<&Path>) -> Result<()> {
    let current: Option<i64> = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .ok();
    if let (Some(version), Some(path)) = (current, path) {
        if version < LATEST_VERSION {
            // バックアップに平文のキーを持ち込まないよう、先に暗号化しておく。
            seal_legacy_secrets(conn)?;
            let dir = path.parent().unwrap_or(Path::new(".")).join("backup");
            std::fs::create_dir_all(&dir)?;
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            let target = dir.join(format!("before-v{LATEST_VERSION}-{stamp}.sqlite"));
            conn.execute("VACUUM INTO ?1", [target.display().to_string()])
                .context("移行前のバックアップを作れません")?;
            tracing::info!("移行の前に DB を残しました: {}", target.display());
            migrate(conn)?;
            // 書き換える前の値（平文のキーなど）が WAL や空きページに残らないよう、詰め直す。
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
                .context("移行後の DB を詰め直せません")?;
            return Ok(());
        }
    }
    migrate(conn)
}

/// 検索のように長く掛かる読み取り用。書き込み用の接続のロックを持たない。
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let has_version: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if has_version == 0 {
        create_fresh(conn)?;
        conn.execute(
            "INSERT INTO settings (id, api_key, model, summary_prompt, knowledge_path, fess_url, fess_token, fess_enabled)
             VALUES (1, '', ?1, ?2, '', '', '', 0)",
            params![DEFAULT_MODEL, DEFAULT_SUMMARY_PROMPT],
        )?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [LATEST_VERSION],
        )?;
        return Ok(());
    }
    let mut current: i64 =
        conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
    // 版ごとの手順を順に当てる。途中で落ちても、各手順は何度当てても同じ結果になる。
    while current < LATEST_VERSION {
        let next = current + 1;
        let tx = conn.unchecked_transaction()?;
        match next {
            2 => ensure_job_retry_columns(&tx)?,
            3 => migrate_v3(&tx)?,
            4 => migrate_v4(&tx)?,
            5 => migrate_v5(&tx)?,
            6 => migrate_v6(&tx)?,
            7 => migrate_v7(&tx)?,
            8 => migrate_v8(&tx)?,
            9 => migrate_v9(&tx)?,
            10 => migrate_v10(&tx)?,
            11 => migrate_v11(&tx)?,
            12 => migrate_v12(&tx)?,
            13 => migrate_v13(&tx)?,
            14 => migrate_v14(&tx)?,
            _ => anyhow::bail!("不明なスキーマ版 {next}"),
        }
        tx.execute("UPDATE schema_version SET version=?1", [next])?;
        tx.commit()?;
        current = next;
    }
    Ok(())
}

fn ensure_job_retry_columns(conn: &Connection) -> Result<()> {
    add_column(conn, "audio_jobs", "attempts", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "audio_jobs", "next_retry_at", "TEXT")?;
    Ok(())
}

/// 版 3: 参加者・手動編集・無音区間の列と、会議を消したときに子の行も消える外部キー。
fn migrate_v3(conn: &Connection) -> Result<()> {
    add_column(conn, "settings", "knowledge_seeded", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "meetings", "participants", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "meetings", "summary_at", "TEXT")?;
    add_column(conn, "questions", "locked", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "audio_jobs", "silent", "INTEGER NOT NULL DEFAULT 0")?;
    // 既存の行のうち、親の会議が無いもの（削除の途中で残った行）は持ち越さない。
    if !has_foreign_key(conn, "segments")? {
        conn.execute_batch(
            "CREATE TABLE segments_v3 (
               id INTEGER PRIMARY KEY,
               meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
               start_sec REAL NOT NULL,
               text TEXT NOT NULL
             );
             INSERT INTO segments_v3 (id, meeting_id, start_sec, text)
               SELECT id, meeting_id, start_sec, text FROM segments
               WHERE meeting_id IN (SELECT id FROM meetings);
             DROP TABLE segments;
             ALTER TABLE segments_v3 RENAME TO segments;",
        )?;
    }
    if !has_foreign_key(conn, "audio_jobs")? {
        conn.execute_batch(
            "CREATE TABLE audio_jobs_v3 (
               id INTEGER PRIMARY KEY,
               meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
               start_ms INTEGER NOT NULL,
               wav_path TEXT NOT NULL,
               status TEXT NOT NULL,
               error TEXT,
               created_at TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0,
               next_retry_at TEXT,
               silent INTEGER NOT NULL DEFAULT 0,
               UNIQUE(meeting_id, start_ms)
             );
             INSERT INTO audio_jobs_v3 (id, meeting_id, start_ms, wav_path, status, error, created_at, attempts, next_retry_at, silent)
               SELECT id, meeting_id, start_ms, wav_path, status, error, created_at, attempts, next_retry_at, silent FROM audio_jobs
               WHERE meeting_id IN (SELECT id FROM meetings);
             DROP TABLE audio_jobs;
             ALTER TABLE audio_jobs_v3 RENAME TO audio_jobs;",
        )?;
    }
    if !has_foreign_key(conn, "knowledge_hits")? {
        conn.execute_batch(
            "CREATE TABLE knowledge_hits_v3 (
               id INTEGER PRIMARY KEY,
               meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
               source TEXT NOT NULL,
               title TEXT NOT NULL,
               locator TEXT NOT NULL,
               snippet TEXT NOT NULL,
               query TEXT NOT NULL,
               pinned INTEGER NOT NULL DEFAULT 0,
               created_at TEXT NOT NULL
             );
             INSERT INTO knowledge_hits_v3 (id, meeting_id, source, title, locator, snippet, query, pinned, created_at)
               SELECT id, meeting_id, source, title, locator, snippet, query, pinned, created_at FROM knowledge_hits
               WHERE meeting_id IN (SELECT id FROM meetings);
             DROP TABLE knowledge_hits;
             ALTER TABLE knowledge_hits_v3 RENAME TO knowledge_hits;",
        )?;
    }
    conn.execute_batch(INDEXES)?;
    Ok(())
}

/// 版 4: 会員制。会員・セッション・プラン・利用量・会員ごとの設定と、データの持ち主の列。
/// 既存の会議などの持ち主は空（NULL）のままにし、運用者が `kikimimi admin assign-legacy` で明示的に移す。
fn migrate_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(V4_TABLES)?;
    add_column(conn, "meetings", "user_id", "INTEGER REFERENCES users(id) ON DELETE CASCADE")?;
    add_column(conn, "meetings", "last_audio_at", "TEXT")?;
    add_column(conn, "templates", "user_id", "INTEGER REFERENCES users(id) ON DELETE CASCADE")?;
    add_column(conn, "knowledge_chunks", "user_id", "INTEGER")?;
    add_column(conn, "audio_jobs", "duration_ms", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "audio_jobs", "bytes", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "questions", "source", "TEXT")?;
    if column_exists(conn, "knowledge_docs", "path")? && !column_exists(conn, "knowledge_docs", "user_id")? {
        conn.execute_batch(
            "CREATE TABLE knowledge_docs_v4 (
               id INTEGER PRIMARY KEY,
               user_id INTEGER,
               path TEXT NOT NULL,
               title TEXT NOT NULL,
               mtime INTEGER NOT NULL,
               error TEXT,
               UNIQUE(user_id, path)
             );
             INSERT INTO knowledge_docs_v4 (path, title, mtime, error) SELECT path, title, mtime, error FROM knowledge_docs;
             DROP TABLE knowledge_docs;
             ALTER TABLE knowledge_docs_v4 RENAME TO knowledge_docs;",
        )?;
    }
    conn.execute_batch(V4_INDEXES)?;
    Ok(())
}

const V4_TABLES: &str = "
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY,
  login_id TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash TEXT NOT NULL UNIQUE,
  csrf TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  revoked_at TEXT
);
CREATE TABLE IF NOT EXISTS plans (
  code TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  registrable INTEGER NOT NULL DEFAULT 0,
  public INTEGER NOT NULL DEFAULT 0,
  limits TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS plan_assignments (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  plan_code TEXT NOT NULL REFERENCES plans(code),
  started_at TEXT NOT NULL,
  ended_at TEXT,
  source TEXT NOT NULL,
  external_ref TEXT
);
CREATE TABLE IF NOT EXISTS user_settings (
  user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  api_key TEXT NOT NULL DEFAULT '',
  model TEXT NOT NULL DEFAULT 'gemini-3.8-flash',
  summary_prompt TEXT NOT NULL DEFAULT '',
  knowledge_path TEXT NOT NULL DEFAULT '',
  fess_url TEXT NOT NULL DEFAULT '',
  fess_token TEXT NOT NULL DEFAULT '',
  fess_enabled INTEGER NOT NULL DEFAULT 0,
  provider TEXT NOT NULL DEFAULT 'gemini',
  openai_api_key TEXT NOT NULL DEFAULT '',
  openai_model TEXT NOT NULL DEFAULT '',
  openai_transcribe_model TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS usage_events (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  key TEXT NOT NULL UNIQUE,
  amount INTEGER NOT NULL,
  period TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS question_evidence (
  id INTEGER PRIMARY KEY,
  question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
  segment_id INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
  quote TEXT NOT NULL,
  created_at TEXT NOT NULL
);
";

const V4_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS meetings_user ON meetings(user_id, status);
CREATE INDEX IF NOT EXISTS templates_user ON templates(user_id);
CREATE INDEX IF NOT EXISTS knowledge_chunks_user ON knowledge_chunks(user_id);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS usage_events_user ON usage_events(user_id, kind, period);
CREATE INDEX IF NOT EXISTS question_evidence_question ON question_evidence(question_id);
";

/// 版 5: 参加者を自動で拾って入れたかどうか（人が編集したら外す）。
fn migrate_v5(conn: &Connection) -> Result<()> {
    add_column(conn, "meetings", "participants_auto", "INTEGER NOT NULL DEFAULT 0")
}

/// 会員制より前の共通設定（`settings`）に平文で残っているキーと FESS トークンを暗号化する。
/// 何度当てても同じ（暗号化済みの値は触らない）。
fn seal_legacy_secrets(conn: &Connection) -> Result<()> {
    let row: Option<(String, String)> = conn
        .query_row("SELECT api_key, fess_token FROM settings WHERE id=1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    let Some((api_key, fess_token)) = row else {
        return Ok(());
    };
    let seal = |value: &str| {
        if value.is_empty() || value.starts_with("v1:") {
            value.to_string()
        } else {
            crate::crypto::seal(value)
        }
    };
    if seal(&api_key) != api_key || seal(&fess_token) != fess_token {
        conn.execute(
            "UPDATE settings SET api_key=?1, fess_token=?2 WHERE id=1",
            params![seal(&api_key), seal(&fess_token)],
        )?;
    }
    Ok(())
}

/// 共通設定の秘密値を読む。暗号化前の平文（古い DB）もそのまま読む。
fn legacy_secret(stored: String) -> Result<String> {
    if stored.starts_with("v1:") {
        crate::crypto::open(&stored)
    } else {
        Ok(stored)
    }
}

/// 版 6: Claude（Anthropic）のキーとモデル、Claude を選んだときに文字起こしを任せる提供元。
/// あわせて、共通設定に平文で残っていたキーを暗号化する。
/// 会員の秘密値は、付加データなしの `v1` から会員IDと項目名に結び付けた `v2` に暗号化し直す。
fn migrate_v6(conn: &Connection) -> Result<()> {
    seal_legacy_secrets(conn)?;
    add_column(conn, "user_settings", "anthropic_api_key", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "user_settings", "anthropic_model", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "user_settings", "transcriber", "TEXT NOT NULL DEFAULT 'gemini'")?;
    for field in USER_SECRETS {
        let rows: Vec<(i64, String)> = conn
            .prepare(&format!("SELECT user_id, {field} FROM user_settings WHERE {field} LIKE 'v1:%'"))?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (user_id, stored) in rows {
            let plain = crate::crypto::open(&stored)
                .with_context(|| format!("会員 {user_id} の {field} を読めません（secret.key を確認してください）"))?;
            conn.execute(
                &format!("UPDATE user_settings SET {field}=?1 WHERE user_id=?2"),
                params![seal_user(user_id, field, &plain), user_id],
            )?;
        }
    }
    Ok(())
}

/// 版 7: フォルダと並び順、会議中の AI のメモ・次に聞くこと・「ここ重要」の印、
/// 聞くことの追加元（人 / AI）と「今回は聞かない」。
fn migrate_v7(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS folders (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  position INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS folders_user ON folders(user_id, position);
CREATE TABLE IF NOT EXISTS meeting_notes (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  segment_id INTEGER REFERENCES segments(id) ON DELETE SET NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS meeting_notes_meeting ON meeting_notes(meeting_id);
CREATE TABLE IF NOT EXISTS meeting_marks (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  at_sec REAL NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS meeting_marks_meeting ON meeting_marks(meeting_id);",
    )?;
    add_column(conn, "meetings", "folder_id", "INTEGER REFERENCES folders(id) ON DELETE SET NULL")?;
    add_column(conn, "meetings", "position", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "meetings", "next_hint", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "questions", "origin", "TEXT NOT NULL DEFAULT 'human'")?;
    add_column(conn, "questions", "skipped_by", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "questions", "skip_reason", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "questions", "restored", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute(
        "UPDATE user_settings SET summary_prompt=?1 WHERE summary_prompt=?2",
        params![DEFAULT_SUMMARY_PROMPT, OLD_SUMMARY_PROMPT_V6],
    )?;
    // 並び順は、これまでの「新しい順」をそのまま初期値にする。
    conn.execute_batch(
        "UPDATE meetings SET position = -id WHERE position = 0;
         CREATE INDEX IF NOT EXISTS meetings_folder ON meetings(user_id, folder_id, position);",
    )?;
    Ok(())
}

/// 版 8: 会議に添付する顧客資料（#97）。本文は区切って持ち、その会議の中だけで検索する。
fn migrate_v8(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meeting_docs (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  ext TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  chunks INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS meeting_docs_meeting ON meeting_docs(meeting_id);
CREATE TABLE IF NOT EXISTS meeting_doc_chunks (
  id INTEGER PRIMARY KEY,
  doc_id INTEGER NOT NULL REFERENCES meeting_docs(id) ON DELETE CASCADE,
  meeting_id INTEGER NOT NULL,
  heading TEXT NOT NULL,
  body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS meeting_doc_chunks_meeting ON meeting_doc_chunks(meeting_id);",
    )?;
    Ok(())
}

/// 版 9: アジェンダ（#99）。議題ごとの目安時間（分）と、いまの議題として話していた時間（録音時間の秒）。
fn migrate_v9(conn: &Connection) -> Result<()> {
    add_column(conn, "questions", "minutes", "INTEGER")?;
    add_column(conn, "questions", "spent_sec", "REAL NOT NULL DEFAULT 0")?;
    add_column(conn, "questions", "active_since", "REAL")?;
    conn.execute(
        "UPDATE user_settings SET summary_prompt=?1 WHERE summary_prompt=?2",
        params![DEFAULT_SUMMARY_PROMPT, OLD_SUMMARY_PROMPT_V8],
    )?;
    Ok(())
}

/// 版 10: リアルタイム文字起こし（速報字幕）の設定（#104）と、発言ごとの話者（#83）。
fn migrate_v10(conn: &Connection) -> Result<()> {
    add_column(conn, "user_settings", "realtime_captions", "TEXT NOT NULL DEFAULT 'off'")?;
    add_column(conn, "segments", "speaker", "TEXT NOT NULL DEFAULT ''")?;
    // 人が付けた話者は、終了後の推定で上書きしない。
    add_column(conn, "segments", "speaker_manual", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

/// 版 11: 会議ごとの文字起こし（api / chrome / off）と、終了時に API を使うかどうか。
fn migrate_v11(conn: &Connection) -> Result<()> {
    add_column(conn, "meetings", "speech", "TEXT NOT NULL DEFAULT 'api'")?;
    add_column(conn, "meetings", "transcript_only", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "meetings", "finish_close", "TEXT NOT NULL DEFAULT ''")?;
    Ok(())
}

/// 版 12: 登録前のメール確認コード。平文のコードは置かず、ソルト付きのハッシュだけを残す。
fn migrate_v12(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS signup_codes (
           email TEXT PRIMARY KEY,
           salt TEXT NOT NULL,
           code_hash TEXT NOT NULL,
           password_hash TEXT NOT NULL,
           expires_at TEXT NOT NULL,
           attempts INTEGER NOT NULL DEFAULT 0,
           created_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// 版 13: パスワード再設定の確認コード。登録用のコードとは別にして、取り違えない。
fn migrate_v13(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reset_codes (
           email TEXT PRIMARY KEY,
           salt TEXT NOT NULL,
           code_hash TEXT NOT NULL,
           expires_at TEXT NOT NULL,
           attempts INTEGER NOT NULL DEFAULT 0,
           created_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// 版 14: Chrome の速報字幕の控え。API で文字起こしする会議でも速報を残し、
/// API の文字起こしが打ち切りになった区間を、要約の前にこの控えで埋める。
fn migrate_v14(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS caption_backup (
           id INTEGER PRIMARY KEY,
           meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
           start_sec REAL NOT NULL,
           text TEXT NOT NULL,
           created_at TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS caption_backup_at ON caption_backup(meeting_id, start_sec);",
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingDoc {
    pub id: i64,
    pub name: String,
    pub bytes: i64,
    pub chunks: i64,
    pub error: Option<String>,
    pub created_at: String,
}

pub const MAX_MEETING_DOCS: i64 = 20;

/// 顧客資料を登録する。本文が読めなかったときは error に理由を入れて残す（原本は取り出せる）。
pub fn add_meeting_doc(
    conn: &Connection,
    meeting_id: i64,
    name: &str,
    ext: &str,
    bytes: i64,
    chunks: &[(String, String)],
    error: Option<&str>,
) -> Result<i64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM meeting_docs WHERE meeting_id=?1", [meeting_id], |r| r.get(0))?;
    if count >= MAX_MEETING_DOCS {
        anyhow::bail!("顧客資料は1会議{MAX_MEETING_DOCS}件までです");
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO meeting_docs (meeting_id, name, ext, bytes, chunks, error, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![meeting_id, name, ext, bytes, chunks.len() as i64, error, now_stamp()],
    )?;
    let id = tx.last_insert_rowid();
    for (heading, body) in chunks {
        tx.execute(
            "INSERT INTO meeting_doc_chunks (doc_id, meeting_id, heading, body) VALUES (?1, ?2, ?3, ?4)",
            params![id, meeting_id, heading, body],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

pub fn list_meeting_docs(conn: &Connection, meeting_id: i64) -> Result<Vec<MeetingDoc>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, bytes, chunks, error, created_at FROM meeting_docs WHERE meeting_id=?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([meeting_id], |row| {
        Ok(MeetingDoc {
            id: row.get(0)?,
            name: row.get(1)?,
            bytes: row.get(2)?,
            chunks: row.get(3)?,
            error: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// 原本のファイル名（保存名）と表示名。無ければ None。
pub fn meeting_doc_file(conn: &Connection, meeting_id: i64, doc_id: i64) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT id || '.' || ext, name FROM meeting_docs WHERE id=?1 AND meeting_id=?2",
            params![doc_id, meeting_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub fn delete_meeting_doc(conn: &Connection, meeting_id: i64, doc_id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM meeting_docs WHERE id=?1 AND meeting_id=?2", params![doc_id, meeting_id])? > 0)
}

/// その会議の顧客資料だけを、検索語ごとに探す（各語 3 件まで、全体で 6 件まで）。
pub fn search_meeting_docs(conn: &Connection, meeting_id: i64, queries: &[String]) -> Result<Vec<crate::knowledge::HitDraft>> {
    let mut stmt = conn.prepare(
        "SELECT d.name, c.heading, c.body FROM meeting_doc_chunks c JOIN meeting_docs d ON d.id = c.doc_id
         WHERE c.meeting_id=?1 AND c.body LIKE ?2 ESCAPE '\\' ORDER BY c.id LIMIT 3",
    )?;
    let mut out: Vec<crate::knowledge::HitDraft> = Vec::new();
    for query in queries {
        let query = query.trim();
        if query.chars().count() < 2 {
            continue;
        }
        let rows = stmt.query_map(params![meeting_id, like_contains(query)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        for row in rows {
            let (name, heading, body) = row?;
            let locator = if heading.is_empty() { format!("顧客資料/{name}") } else { format!("顧客資料/{name}#{heading}") };
            if out.iter().any(|hit| hit.locator == locator) {
                continue;
            }
            out.push(crate::knowledge::HitDraft {
                source: "customer".into(),
                title: if heading.is_empty() { name.clone() } else { format!("{name} — {heading}") },
                locator,
                snippet: crate::knowledge::make_snippet(&body, query),
                query: query.to_string(),
            });
        }
        if out.len() >= 6 {
            break;
        }
    }
    out.truncate(6);
    Ok(out)
}

fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    if !column_exists(conn, table, column)? {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_foreign_key(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

const INDEXES: &str = "
CREATE INDEX IF NOT EXISTS segments_meeting ON segments(meeting_id, start_sec);
CREATE INDEX IF NOT EXISTS audio_jobs_status ON audio_jobs(status, meeting_id);
CREATE INDEX IF NOT EXISTS knowledge_hits_meeting ON knowledge_hits(meeting_id, pinned);
CREATE INDEX IF NOT EXISTS questions_meeting ON questions(meeting_id, position);
";

const SCHEMA: &str = r#"
CREATE TABLE schema_version (version INTEGER NOT NULL);
CREATE TABLE settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  api_key TEXT NOT NULL DEFAULT '',
  model TEXT NOT NULL DEFAULT 'gemini-3.8-flash',
  summary_prompt TEXT NOT NULL DEFAULT '',
  knowledge_path TEXT NOT NULL DEFAULT '',
  fess_url TEXT NOT NULL DEFAULT '',
  fess_token TEXT NOT NULL DEFAULT '',
  fess_enabled INTEGER NOT NULL DEFAULT 0,
  knowledge_seeded INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE templates (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE template_questions (
  id INTEGER PRIMARY KEY,
  template_id INTEGER NOT NULL REFERENCES templates(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  body TEXT NOT NULL
);
CREATE TABLE meetings (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  started_at TEXT,
  ended_at TEXT,
  summary_md TEXT NOT NULL DEFAULT '',
  duration_sec REAL NOT NULL DEFAULT 0,
  finish_requested INTEGER NOT NULL DEFAULT 0,
  active_question_id INTEGER,
  error TEXT,
  participants TEXT NOT NULL DEFAULT '',
  summary_at TEXT
);
CREATE TABLE questions (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  body TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  answer TEXT NOT NULL DEFAULT '',
  locked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE segments (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  start_sec REAL NOT NULL,
  text TEXT NOT NULL
);
CREATE TABLE audio_jobs (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  start_ms INTEGER NOT NULL,
  wav_path TEXT NOT NULL,
  status TEXT NOT NULL,
  error TEXT,
  created_at TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_retry_at TEXT,
  silent INTEGER NOT NULL DEFAULT 0,
  UNIQUE(meeting_id, start_ms)
);
CREATE TABLE knowledge_docs (
  path TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  mtime INTEGER NOT NULL,
  error TEXT
);
CREATE TABLE knowledge_chunks (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL,
  title TEXT NOT NULL,
  heading TEXT NOT NULL,
  body TEXT NOT NULL
);
CREATE VIRTUAL TABLE knowledge_fts USING fts5(
  heading,
  body,
  tokenize = 'trigram'
);
CREATE TABLE knowledge_hits (
  id INTEGER PRIMARY KEY,
  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  source TEXT NOT NULL,
  title TEXT NOT NULL,
  locator TEXT NOT NULL,
  snippet TEXT NOT NULL,
  query TEXT NOT NULL,
  pinned INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);
CREATE INDEX segments_meeting ON segments(meeting_id, start_sec);
CREATE INDEX audio_jobs_status ON audio_jobs(status, meeting_id);
CREATE INDEX knowledge_hits_meeting ON knowledge_hits(meeting_id, pinned);
CREATE INDEX questions_meeting ON questions(meeting_id, position);
"#;

/// 新しい DB は版 3 の表を作ってから版 4 の手順を当てる（同じ定義を二か所に書かない）。
fn create_fresh(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    migrate_v4(conn)?;
    migrate_v5(conn)?;
    migrate_v6(conn)?;
    migrate_v7(conn)?;
    migrate_v8(conn)?;
    migrate_v9(conn)?;
    migrate_v10(conn)?;
    migrate_v11(conn)?;
    migrate_v12(conn)?;
    migrate_v13(conn)?;
    migrate_v14(conn)
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Settings {
    pub api_key: String,
    pub model: String,
    pub summary_prompt: String,
    pub knowledge_path: String,
    pub fess_url: String,
    pub fess_token: String,
    pub fess_enabled: bool,
    /// 回答・要約に使う提供元（"gemini" / "openai" / "anthropic"）。
    pub provider: String,
    pub openai_api_key: String,
    pub openai_model: String,
    pub openai_transcribe_model: String,
    pub anthropic_api_key: String,
    pub anthropic_model: String,
    /// Claude は音声を受け付けないため、Claude を選んだときに文字起こしを任せる提供元（"gemini" / "openai"）。
    pub transcriber: String,
    /// リアルタイム文字起こし（Chrome の速報字幕）: "off" / "local"（端末内）/ "cloud"（Google に送る、運用者が許可したときだけ）。
    pub realtime_captions: String,
}

impl Settings {
    pub fn uses_openai(&self) -> bool {
        self.provider == "openai"
    }

    pub fn uses_anthropic(&self) -> bool {
        self.provider == "anthropic"
    }

    /// 文字起こしに使う提供元。Claude のときは transcriber、それ以外は provider と同じ。
    pub fn transcribe_provider(&self) -> &str {
        if self.uses_anthropic() {
            if self.transcriber == "openai" { "openai" } else { "gemini" }
        } else if self.uses_openai() {
            "openai"
        } else {
            "gemini"
        }
    }

    /// 録音を文字にするキーがあるか。Claude のときは文字起こし用の提供元を見る。
    pub fn has_transcribe_key(&self) -> bool {
        match self.transcribe_provider() {
            "openai" => !self.openai_api_key.is_empty(),
            _ => !self.api_key.is_empty(),
        }
    }

    /// 書き起こしを議事録にまとめるキーがあるか。
    pub fn has_chat_key(&self) -> bool {
        if self.uses_anthropic() {
            !self.anthropic_api_key.is_empty()
        } else if self.uses_openai() {
            !self.openai_api_key.is_empty()
        } else {
            !self.api_key.is_empty()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStats {
    pub docs: i64,
    pub chunks: i64,
    pub errors: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexError {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Meeting {
    pub id: i64,
    pub title: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub summary_md: String,
    pub summary_at: Option<String>,
    pub duration_sec: f64,
    pub finish_requested: bool,
    pub active_question_id: Option<i64>,
    pub error: Option<String>,
    pub participants: String,
    /// 参加者は要約のときに書き起こしから拾ったもの（人が編集していない）。
    pub participants_auto: bool,
    pub folder_id: Option<i64>,
    pub position: i64,
    /// 会議中に AI が出した「次に聞くとよいこと」。
    pub next_hint: String,
    pub pending_jobs: i64,
    pub retry_waiting: i64,
    pub next_retry_at: Option<String>,
    pub failed_jobs: i64,
    /// 録音中の文字起こし。"api"（APIキー）/ "chrome"（Chrome の音声認識）/ "off"（録音だけ）。
    pub speech: String,
    /// API の文字起こしを、回答やナレッジを付けずに文字だけにする。
    pub transcript_only: bool,
    /// 終了のしかた。"" は従来どおり、"api" / "chrome" / "plain"。
    pub finish_close: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Question {
    pub id: i64,
    pub position: i64,
    pub body: String,
    pub status: String,
    pub answer: String,
    /// 人が確認・修正した。以後 AI は上書きしない。
    pub locked: bool,
    /// 最後に回答を書いたのは誰か（"ai" / "human"）。
    pub source: Option<String>,
    /// AI が回答に使った発言。新しい順。
    pub evidence: Vec<Evidence>,
    /// この聞くことを足したのは誰か（"human" / "ai"）。
    pub origin: String,
    /// 「今回は聞かない」にしたのは誰か（"human" / "ai"）。空なら外していない。
    pub skipped_by: String,
    pub skip_reason: String,
    /// 議題の目安時間（分）。無ければ None。
    pub minutes: Option<i64>,
    /// いまの議題として話していた時間（録音時間の秒、確定した分）。
    pub spent_sec: f64,
    /// いまの議題になった録音時間（秒）。いまの議題でなければ None。
    pub active_since: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub segment_id: i64,
    pub start_sec: f64,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Segment {
    pub id: i64,
    pub start_sec: f64,
    pub text: String,
    /// 話者（#83）。会議の終了後に AI が推定するか、人が付ける。無ければ空。
    pub speaker: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: i64,
    pub source: String,
    pub title: String,
    pub locator: String,
    pub snippet: String,
    pub query: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Template {
    pub id: i64,
    pub name: String,
    pub questions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AudioJob {
    pub id: i64,
    pub meeting_id: i64,
    pub start_ms: i64,
    pub wav_path: String,
}

#[derive(Debug, Clone)]
pub struct AnswerUpdate {
    pub question_id: i64,
    pub candidate: String,
    pub status: String,
    /// 回答の根拠にした発言の抜粋。無ければ空。
    pub evidence: String,
}

pub fn get_settings(conn: &Connection) -> Result<Settings> {
    let mut settings = conn.query_row(
        "SELECT api_key, model, summary_prompt, knowledge_path, fess_url, fess_token, fess_enabled FROM settings WHERE id=1",
        [],
        |row| {
            Ok(Settings {
                api_key: row.get(0)?,
                model: row.get(1)?,
                summary_prompt: row.get(2)?,
                knowledge_path: row.get(3)?,
                fess_url: row.get(4)?,
                fess_token: row.get(5)?,
                fess_enabled: row.get::<_, i64>(6)? != 0,
                provider: "gemini".into(),
                openai_api_key: String::new(),
                openai_model: String::new(),
                openai_transcribe_model: String::new(),
                ..Default::default()
            })
        },
    )
    .context("settings")?;
    settings.api_key = legacy_secret(std::mem::take(&mut settings.api_key))?;
    settings.fess_token = legacy_secret(std::mem::take(&mut settings.fess_token))?;
    Ok(settings)
}

#[derive(Default)]
pub struct SettingsPatch {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub summary_prompt: Option<String>,
    pub knowledge_path: Option<String>,
    pub fess_url: Option<String>,
    pub fess_token: Option<String>,
    pub fess_enabled: Option<bool>,
    pub provider: Option<String>,
    pub openai_api_key: Option<String>,
    pub openai_model: Option<String>,
    pub openai_transcribe_model: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub anthropic_model: Option<String>,
    pub transcriber: Option<String>,
    pub realtime_captions: Option<String>,
    pub clear_anthropic_api_key: bool,
    /// 空文字のキー / トークンは「変更しない」。消すときはこちらを立てる。
    pub clear_api_key: bool,
    pub clear_openai_api_key: bool,
    pub clear_fess_token: bool,
}

/// 版 4 より前の共通設定（移行の元データ）。会員の設定は `patch_user_settings`。
#[cfg(test)]
pub fn patch_settings(conn: &Connection, patch: SettingsPatch) -> Result<Settings> {
    let mut current = get_settings(conn)?;
    apply_patch(&mut current, patch);
    conn.execute(
        "UPDATE settings SET api_key=?1, model=?2, summary_prompt=?3, knowledge_path=?4, fess_url=?5, fess_token=?6, fess_enabled=?7 WHERE id=1",
        params![
            current.api_key,
            current.model,
            current.summary_prompt,
            current.knowledge_path,
            current.fess_url,
            current.fess_token,
            current.fess_enabled as i64
        ],
    )?;
    Ok(current)
}

/// 会員の設定。API キーと FESS トークンは暗号化して保存し、読むときに戻す。
pub fn get_user_settings(conn: &Connection, user_id: i64) -> Result<Settings> {
    let row = conn
        .query_row(
            "SELECT api_key, model, summary_prompt, knowledge_path, fess_url, fess_token, fess_enabled,
                    provider, openai_api_key, openai_model, openai_transcribe_model,
                    anthropic_api_key, anthropic_model, transcriber, realtime_captions
             FROM user_settings WHERE user_id=?1",
            [user_id],
            |row| {
                Ok((
                    (
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)? != 0,
                    ),
                    (
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ),
                    (
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, String>(13)?,
                        row.get::<_, String>(14)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((
        (api_key, model, summary_prompt, knowledge_path, fess_url, fess_token, fess_enabled),
        (provider, openai_api_key, openai_model, openai_transcribe_model),
        (anthropic_api_key, anthropic_model, transcriber, realtime_captions),
    )) = row
    else {
        return Ok(Settings {
            api_key: String::new(),
            model: DEFAULT_MODEL.into(),
            summary_prompt: DEFAULT_SUMMARY_PROMPT.into(),
            knowledge_path: String::new(),
            fess_url: String::new(),
            fess_token: String::new(),
            fess_enabled: false,
            provider: "gemini".into(),
            openai_api_key: String::new(),
            openai_model: String::new(),
            openai_transcribe_model: String::new(),
            anthropic_api_key: String::new(),
            anthropic_model: String::new(),
            transcriber: "gemini".into(),
            realtime_captions: "off".into(),
        });
    };
    Ok(Settings {
        provider,
        realtime_captions,
        anthropic_api_key: open_user(user_id, "anthropic_api_key", &anthropic_api_key)?,
        anthropic_model,
        transcriber,
        openai_api_key: open_user(user_id, "openai_api_key", &openai_api_key)?,
        openai_model,
        openai_transcribe_model,
        api_key: open_user(user_id, "api_key", &api_key)?,
        model,
        summary_prompt: if summary_prompt.trim().is_empty() {
            DEFAULT_SUMMARY_PROMPT.into()
        } else {
            summary_prompt
        },
        knowledge_path,
        fess_url,
        fess_token: open_user(user_id, "fess_token", &fess_token)?,
        fess_enabled,
    })
}

/// 会員の秘密値は、会員IDと項目名に結び付けて暗号化する（別の会員・項目へ移すと読めない）。
fn seal_user(user_id: i64, field: &str, plain: &str) -> String {
    crate::crypto::seal_bound(plain, &crate::crypto::user_context(user_id, field))
}

fn open_user(user_id: i64, field: &str, stored: &str) -> Result<String> {
    crate::crypto::open_bound(stored, &crate::crypto::user_context(user_id, field))
        .with_context(|| format!("会員 {user_id} の {field} を読めません"))
}

/// 会員の秘密値の列。
const USER_SECRETS: [&str; 4] = ["api_key", "openai_api_key", "anthropic_api_key", "fess_token"];

pub fn patch_user_settings(conn: &Connection, user_id: i64, patch: SettingsPatch) -> Result<Settings> {
    let tx = conn.unchecked_transaction()?;
    let current = patch_user_settings_in(&tx, user_id, patch)?;
    tx.commit()?;
    Ok(current)
}

/// トランザクションの中で使う版。
fn patch_user_settings_in(tx: &Connection, user_id: i64, patch: SettingsPatch) -> Result<Settings> {
    let mut current = get_user_settings(tx, user_id)?;
    let old_path = current.knowledge_path.clone();
    apply_patch(&mut current, patch);
    tx.execute(
        "INSERT INTO user_settings (user_id, api_key, model, summary_prompt, knowledge_path, fess_url, fess_token, fess_enabled,
                                    provider, openai_api_key, openai_model, openai_transcribe_model,
                                    anthropic_api_key, anthropic_model, transcriber, realtime_captions)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(user_id) DO UPDATE SET api_key=excluded.api_key, model=excluded.model, summary_prompt=excluded.summary_prompt,
           knowledge_path=excluded.knowledge_path, fess_url=excluded.fess_url, fess_token=excluded.fess_token, fess_enabled=excluded.fess_enabled,
           provider=excluded.provider, openai_api_key=excluded.openai_api_key, openai_model=excluded.openai_model,
           openai_transcribe_model=excluded.openai_transcribe_model, anthropic_api_key=excluded.anthropic_api_key,
           anthropic_model=excluded.anthropic_model, transcriber=excluded.transcriber,
           realtime_captions=excluded.realtime_captions",
        params![
            user_id,
            seal_user(user_id, "api_key", &current.api_key),
            current.model,
            current.summary_prompt,
            current.knowledge_path,
            current.fess_url,
            seal_user(user_id, "fess_token", &current.fess_token),
            current.fess_enabled as i64,
            current.provider,
            seal_user(user_id, "openai_api_key", &current.openai_api_key),
            current.openai_model,
            current.openai_transcribe_model,
            seal_user(user_id, "anthropic_api_key", &current.anthropic_api_key),
            current.anthropic_model,
            current.transcriber,
            current.realtime_captions
        ],
    )?;
    if current.knowledge_path != old_path {
        clear_index(tx, user_id)?;
    }
    Ok(current)
}

fn apply_patch(current: &mut Settings, patch: SettingsPatch) {
    if let Some(key) = patch.api_key {
        let key = key.trim().to_string();
        if !key.is_empty() {
            current.api_key = key;
        }
    }
    if patch.clear_api_key {
        current.api_key.clear();
    }
    if let Some(model) = patch.model {
        if !model.trim().is_empty() {
            current.model = model.trim().to_string();
        }
    }
    if let Some(prompt) = patch.summary_prompt {
        current.summary_prompt = if prompt.trim().is_empty() {
            DEFAULT_SUMMARY_PROMPT.to_string()
        } else {
            prompt
        };
    }
    if let Some(path) = patch.knowledge_path {
        current.knowledge_path = path.trim().to_string();
    }
    if let Some(url) = patch.fess_url {
        current.fess_url = url.trim().trim_end_matches('/').to_string();
    }
    if let Some(token) = patch.fess_token {
        if !token.trim().is_empty() {
            current.fess_token = token.trim().to_string();
        }
    }
    if patch.clear_fess_token {
        current.fess_token.clear();
    }
    if let Some(enabled) = patch.fess_enabled {
        current.fess_enabled = enabled;
    }
    if let Some(provider) = patch.provider {
        if matches!(provider.as_str(), "gemini" | "openai" | "anthropic") {
            current.provider = provider;
        }
    }
    if let Some(key) = patch.openai_api_key {
        let key = key.trim().to_string();
        if !key.is_empty() {
            current.openai_api_key = key;
        }
    }
    if patch.clear_openai_api_key {
        current.openai_api_key.clear();
    }
    if let Some(model) = patch.openai_model {
        current.openai_model = model.trim().to_string();
    }
    if let Some(model) = patch.openai_transcribe_model {
        current.openai_transcribe_model = model.trim().to_string();
    }
    if let Some(key) = patch.anthropic_api_key {
        let key = key.trim().to_string();
        if !key.is_empty() {
            current.anthropic_api_key = key;
        }
    }
    if patch.clear_anthropic_api_key {
        current.anthropic_api_key.clear();
    }
    if let Some(model) = patch.anthropic_model {
        current.anthropic_model = model.trim().to_string();
    }
    if let Some(transcriber) = patch.transcriber {
        if matches!(transcriber.as_str(), "gemini" | "openai") {
            current.transcriber = transcriber;
        }
    }
    if let Some(mode) = patch.realtime_captions {
        if matches!(mode.as_str(), "off" | "local" | "cloud") {
            current.realtime_captions = mode;
        }
    }
}

/// その会員の索引だけを消す。
pub fn clear_index(conn: &Connection, user_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM knowledge_fts WHERE rowid IN (SELECT id FROM knowledge_chunks WHERE user_id=?1)",
        [user_id],
    )?;
    conn.execute("DELETE FROM knowledge_chunks WHERE user_id=?1", [user_id])?;
    conn.execute("DELETE FROM knowledge_docs WHERE user_id=?1", [user_id])?;
    Ok(())
}

pub fn index_stats(conn: &Connection, user_id: i64) -> Result<IndexStats> {
    let docs: i64 =
        conn.query_row("SELECT COUNT(*) FROM knowledge_docs WHERE user_id=?1", [user_id], |r| r.get(0))?;
    let chunks: i64 = conn.query_row(
        "SELECT COUNT(*) FROM knowledge_chunks WHERE user_id=?1",
        [user_id],
        |r| r.get(0),
    )?;
    let errors: i64 = conn.query_row(
        "SELECT COUNT(*) FROM knowledge_docs WHERE user_id=?1 AND error IS NOT NULL",
        [user_id],
        |r| r.get(0),
    )?;
    Ok(IndexStats {
        docs,
        chunks,
        errors,
    })
}

pub fn index_errors(conn: &Connection, user_id: i64) -> Result<Vec<IndexError>> {
    let mut stmt = conn.prepare(
        "SELECT path, error FROM knowledge_docs WHERE user_id=?1 AND error IS NOT NULL ORDER BY path LIMIT 100",
    )?;
    let rows = stmt.query_map([user_id], |row| {
        Ok(IndexError {
            path: row.get(0)?,
            error: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn list_templates(conn: &Connection, user_id: i64) -> Result<Vec<Template>> {
    let mut stmt = conn.prepare("SELECT id, name FROM templates WHERE user_id=?1 ORDER BY id DESC")?;
    let ids: Vec<(i64, String)> = stmt
        .query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, name) in ids {
        out.push(Template {
            id,
            name,
            questions: template_questions(conn, id)?,
        });
    }
    Ok(out)
}

fn template_questions(conn: &Connection, id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT body FROM template_questions WHERE template_id=?1 ORDER BY position, id",
    )?;
    let rows = stmt.query_map([id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn create_template(
    conn: &Connection,
    user_id: i64,
    name: &str,
    questions: &[String],
) -> Result<Template> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO templates (user_id, name, created_at) VALUES (?1, ?2, ?3)",
        params![user_id, name, now_stamp()],
    )?;
    let id = tx.last_insert_rowid();
    insert_template_questions(&tx, id, questions)?;
    tx.commit()?;
    Ok(Template {
        id,
        name: name.to_string(),
        questions: questions.to_vec(),
    })
}

fn insert_template_questions(conn: &Connection, id: i64, questions: &[String]) -> Result<()> {
    for (pos, body) in questions.iter().enumerate() {
        conn.execute(
            "INSERT INTO template_questions (template_id, position, body) VALUES (?1, ?2, ?3)",
            params![id, pos as i64, body],
        )?;
    }
    Ok(())
}

pub fn update_template(
    conn: &Connection,
    user_id: i64,
    id: i64,
    name: &str,
    questions: &[String],
) -> Result<Option<Template>> {
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE templates SET name=?1 WHERE id=?2 AND user_id=?3",
        params![name, id, user_id],
    )?;
    if n == 0 {
        return Ok(None);
    }
    tx.execute("DELETE FROM template_questions WHERE template_id=?1", [id])?;
    insert_template_questions(&tx, id, questions)?;
    tx.commit()?;
    Ok(Some(Template {
        id,
        name: name.to_string(),
        questions: questions.to_vec(),
    }))
}

pub fn delete_template(conn: &Connection, user_id: i64, id: i64) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM templates WHERE id=?1 AND user_id=?2",
        params![id, user_id],
    )?;
    Ok(n > 0)
}

pub fn delete_meeting(conn: &Connection, id: i64) -> Result<bool> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM meetings WHERE id=?1",
            [id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !exists {
        return Ok(false);
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM knowledge_hits WHERE meeting_id=?1", [id])?;
    tx.execute("DELETE FROM audio_jobs WHERE meeting_id=?1", [id])?;
    tx.execute("DELETE FROM segments WHERE meeting_id=?1", [id])?;
    tx.execute("DELETE FROM caption_backup WHERE meeting_id=?1", [id])?;
    tx.execute("DELETE FROM meetings WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(true)
}

pub fn create_meeting(
    conn: &Connection,
    user_id: i64,
    title: &str,
    questions: &[String],
) -> Result<i64> {
    let tx = conn.unchecked_transaction()?;
    // 新しい会議は一覧のいちばん上（並び順の最小値より前）に置く。
    let top: i64 = tx.query_row(
        "SELECT COALESCE(MIN(position), 0) - 1 FROM meetings WHERE user_id=?1",
        [user_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO meetings (user_id, title, status, created_at, position) VALUES (?1, ?2, 'draft', ?3, ?4)",
        params![user_id, title, now_stamp(), top],
    )?;
    let id = tx.last_insert_rowid();
    insert_questions(&tx, id, questions)?;
    tx.commit()?;
    Ok(id)
}

fn insert_questions(conn: &Connection, meeting_id: i64, questions: &[String]) -> Result<()> {
    for (pos, body) in questions.iter().enumerate() {
        conn.execute(
            "INSERT INTO questions (meeting_id, position, body) VALUES (?1, ?2, ?3)",
            params![meeting_id, pos as i64, body],
        )?;
    }
    Ok(())
}

/// 議題の目安時間（分）を、並び順どおりに入れる（#99）。None・0 は目安なし。
pub fn set_agenda_minutes(conn: &Connection, meeting_id: i64, minutes: &[Option<i64>]) -> Result<()> {
    let ids: Vec<i64> = list_questions(conn, meeting_id)?.iter().map(|q| q.id).collect();
    for (id, value) in ids.iter().zip(minutes) {
        let value = value.filter(|m| *m > 0).map(|m| m.min(24 * 60));
        conn.execute("UPDATE questions SET minutes=?1 WHERE id=?2", params![value, id])?;
    }
    Ok(())
}

pub fn replace_draft_questions(
    conn: &Connection,
    meeting_id: i64,
    title: &str,
    questions: &[String],
) -> Result<()> {
    let status: String = conn
        .query_row(
            "SELECT status FROM meetings WHERE id=?1",
            [meeting_id],
            |r| r.get(0),
        )
        .optional()?
        .context("会議がありません")?;
    if status != "draft" {
        anyhow::bail!("下書きの会議だけ編集できます");
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE meetings SET title=?1 WHERE id=?2",
        params![title, meeting_id],
    )?;
    tx.execute("DELETE FROM questions WHERE meeting_id=?1", [meeting_id])?;
    insert_questions(&tx, meeting_id, questions)?;
    tx.commit()?;
    Ok(())
}

/// タイトルと参加者は、会議のどの段階でも変えられる。
pub fn update_meeting_info(
    conn: &Connection,
    meeting_id: i64,
    title: Option<&str>,
    participants: Option<&str>,
) -> Result<()> {
    meeting_status(conn, meeting_id)?;
    if let Some(title) = title {
        conn.execute(
            "UPDATE meetings SET title=?1 WHERE id=?2",
            params![title, meeting_id],
        )?;
    }
    if let Some(participants) = participants {
        // 人が入れた参加者。以後の要約で拾い直さない。
        conn.execute(
            "UPDATE meetings SET participants=?1, participants_auto=0 WHERE id=?2",
            params![participants, meeting_id],
        )?;
    }
    Ok(())
}

/// 要約のときに書き起こしから拾った参加者を入れる。人が入れた参加者があれば何もしない（false）。
pub fn set_auto_participants(conn: &Connection, meeting_id: i64, participants: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE meetings SET participants=?1, participants_auto=1
         WHERE id=?2 AND (participants='' OR participants_auto=1)",
        params![participants, meeting_id],
    )?;
    Ok(n > 0)
}

pub struct MeetingFilter<'a> {
    pub query: &'a str,
    pub status: &'a str,
    /// "" はすべて、"none" はフォルダなし、数字はそのフォルダ。
    pub folder: &'a str,
}

/// 会議の持ち主。会議が無ければ None、持ち主が未設定（移行前のデータ）なら Some(None)。
pub fn meeting_owner(conn: &Connection, meeting_id: i64) -> Result<Option<Option<i64>>> {
    Ok(conn
        .query_row("SELECT user_id FROM meetings WHERE id=?1", [meeting_id], |r| r.get(0))
        .optional()?)
}

pub fn list_meetings(
    conn: &Connection,
    user_id: i64,
    filter: &MeetingFilter,
) -> Result<(i64, i64, Vec<Meeting>)> {
    let total: i64 =
        conn.query_row("SELECT COUNT(*) FROM meetings WHERE user_id=?1", [user_id], |r| r.get(0))?;
    let done: i64 = conn.query_row(
        "SELECT COUNT(*) FROM meetings WHERE user_id=?1 AND status='done'",
        [user_id],
        |r| r.get(0),
    )?;
    let query = filter.query.trim();
    let pattern = like_contains(query);
    let status = filter.status.trim();
    let mut stmt = conn.prepare(
        "SELECT id FROM meetings m
         WHERE m.user_id = ?4
           AND (?1 = '' OR m.status = ?1 OR (?1 = 'active' AND m.status IN ('live','paused','wrapping')))
           AND (?2 = '' OR m.title LIKE ?3 ESCAPE '\\' OR m.summary_md LIKE ?3 ESCAPE '\\'
                OR m.participants LIKE ?3 ESCAPE '\\'
                OR EXISTS (SELECT 1 FROM segments s WHERE s.meeting_id = m.id AND s.text LIKE ?3 ESCAPE '\\'))
           AND (?5 = '' OR (?5 = 'none' AND m.folder_id IS NULL) OR CAST(m.folder_id AS TEXT) = ?5)
         ORDER BY m.position, m.id DESC",
    )?;
    let folder = filter.folder.trim();
    let ids: Vec<i64> = stmt
        .query_map(params![status, query, pattern, user_id, folder], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut meetings = Vec::new();
    for id in ids {
        if let Some(meeting) = get_meeting(conn, id)? {
            meetings.push(meeting);
        }
    }
    Ok((total, done, meetings))
}

fn like_contains(query: &str) -> String {
    let mut pattern = String::from('%');
    for ch in query.chars() {
        if ch == '%' || ch == '_' || ch == '\\' {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

pub fn get_meeting(conn: &Connection, id: i64) -> Result<Option<Meeting>> {
    let row = conn
        .query_row(
            "SELECT id, title, status, created_at, started_at, ended_at, summary_md, duration_sec, finish_requested, active_question_id, error, participants, summary_at, participants_auto, folder_id, position, next_hint, speech, transcript_only, finish_close
             FROM meetings WHERE id=?1",
            [id],
            |row| {
                Ok(Meeting {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    started_at: row.get(4)?,
                    ended_at: row.get(5)?,
                    summary_md: row.get(6)?,
                    duration_sec: row.get(7)?,
                    finish_requested: row.get::<_, i64>(8)? != 0,
                    active_question_id: row.get(9)?,
                    error: row.get(10)?,
                    participants: row.get(11)?,
                    summary_at: row.get(12)?,
                    participants_auto: row.get::<_, i64>(13)? != 0,
                    folder_id: row.get(14)?,
                    position: row.get(15)?,
                    next_hint: row.get(16)?,
                    pending_jobs: 0,
                    retry_waiting: 0,
                    next_retry_at: None,
                    failed_jobs: 0,
                    speech: row.get(17)?,
                    transcript_only: row.get::<_, i64>(18)? != 0,
                    finish_close: row.get(19)?,
                })
            },
        )
        .optional()?;
    let Some(mut meeting) = row else {
        return Ok(None);
    };
    meeting.pending_jobs = pending_jobs(conn, id)?;
    let (waiting, next) = retry_waiting(conn, id)?;
    meeting.retry_waiting = waiting;
    meeting.next_retry_at = next;
    meeting.failed_jobs = count_error_jobs(conn, id)?;
    Ok(Some(meeting))
}

/// 文字起こし待ち（未処理と処理中）の区間数。再試行待ちは `retry_waiting` で数える。
pub fn pending_jobs(conn: &Connection, meeting_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM audio_jobs WHERE meeting_id=?1 AND status IN ('pending','processing')",
        [meeting_id],
        |r| r.get(0),
    )?)
}

/// 時刻を置いて再試行する予定の区間数と、いちばん早い再試行時刻。
pub fn retry_waiting(conn: &Connection, meeting_id: i64) -> Result<(i64, Option<String>)> {
    Ok(conn.query_row(
        "SELECT COUNT(*), MIN(next_retry_at) FROM audio_jobs
         WHERE meeting_id=?1 AND status='error' AND next_retry_at IS NOT NULL AND attempts<?2",
        params![meeting_id, MAX_ATTEMPTS],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// 終了処理を待たせる区間数。未処理・処理中・再試行待ちのすべて。
pub fn unfinished_jobs(conn: &Connection, meeting_id: i64) -> Result<i64> {
    Ok(pending_jobs(conn, meeting_id)? + retry_waiting(conn, meeting_id)?.0)
}

pub fn list_questions(conn: &Connection, meeting_id: i64) -> Result<Vec<Question>> {
    let mut stmt = conn.prepare(
        "SELECT id, position, body, status, answer, locked, source, origin, skipped_by, skip_reason, minutes, spent_sec, active_since
         FROM questions WHERE meeting_id=?1 ORDER BY position, id",
    )?;
    let rows = stmt.query_map([meeting_id], |row| {
        Ok(Question {
            id: row.get(0)?,
            position: row.get(1)?,
            body: row.get(2)?,
            status: row.get(3)?,
            answer: row.get(4)?,
            locked: row.get::<_, i64>(5)? != 0,
            source: row.get(6)?,
            evidence: Vec::new(),
            origin: row.get(7)?,
            skipped_by: row.get(8)?,
            skip_reason: row.get(9)?,
            minutes: row.get(10)?,
            spent_sec: row.get(11)?,
            active_since: row.get(12)?,
        })
    })?;
    let mut questions: Vec<Question> = rows.collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare(
        "SELECT e.question_id, e.segment_id, s.start_sec, e.quote FROM question_evidence e
         JOIN segments s ON s.id = e.segment_id JOIN questions q ON q.id = e.question_id
         WHERE q.meeting_id=?1 ORDER BY e.id DESC",
    )?;
    let rows = stmt.query_map([meeting_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            Evidence {
                segment_id: row.get(1)?,
                start_sec: row.get(2)?,
                quote: row.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (question_id, evidence) = row?;
        if let Some(question) = questions.iter_mut().find(|q| q.id == question_id) {
            if question.evidence.len() < 5 {
                question.evidence.push(evidence);
            }
        }
    }
    Ok(questions)
}

pub fn add_question(conn: &Connection, meeting_id: i64, body: &str) -> Result<()> {
    meeting_status(conn, meeting_id)?;
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM questions WHERE meeting_id=?1",
        [meeting_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO questions (meeting_id, position, body) VALUES (?1, ?2, ?3)",
        params![meeting_id, next, body],
    )?;
    Ok(())
}

pub struct QuestionEdit<'a> {
    pub body: Option<&'a str>,
    pub answer: Option<&'a str>,
    pub status: Option<&'a str>,
    /// true: 人が内容を確認した（以後 AI は上書きしない）。false: 確認を外し、AI の更新に戻す。
    pub confirmed: Option<bool>,
    /// true: 「今回は聞かない」にする。false: 聞く対象に戻す（以後 AI はこの質問を外さない）。
    pub skip: Option<bool>,
    /// 目安時間（分）。Some(0) で目安なしに戻す。
    pub minutes: Option<i64>,
}

/// 人が直した回答と状態は、以後の自動更新で上書きしない（locked）。
pub fn edit_question(
    conn: &Connection,
    meeting_id: i64,
    question_id: i64,
    edit: &QuestionEdit,
) -> Result<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT id FROM questions WHERE id=?1 AND meeting_id=?2",
            params![question_id, meeting_id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        anyhow::bail!("質問が見つかりません");
    }
    if let Some(body) = edit.body {
        conn.execute(
            "UPDATE questions SET body=?1 WHERE id=?2",
            params![body, question_id],
        )?;
    }
    if let Some(answer) = edit.answer {
        conn.execute(
            "UPDATE questions SET answer=?1, locked=1, source='human' WHERE id=?2",
            params![answer, question_id],
        )?;
    }
    if let Some(status) = edit.status {
        if !matches!(status, "open" | "draft" | "sufficient") {
            anyhow::bail!("状態が不正です");
        }
        conn.execute(
            "UPDATE questions SET status=?1, locked=1 WHERE id=?2",
            params![status, question_id],
        )?;
    }
    if let Some(confirmed) = edit.confirmed {
        conn.execute(
            "UPDATE questions SET locked=?1 WHERE id=?2",
            params![confirmed as i64, question_id],
        )?;
    }
    if let Some(minutes) = edit.minutes {
        let value = (minutes > 0).then_some(minutes.min(24 * 60));
        conn.execute("UPDATE questions SET minutes=?1 WHERE id=?2", params![value, question_id])?;
    }
    match edit.skip {
        Some(true) => {
            conn.execute(
                "UPDATE questions SET status='skipped', skipped_by='human', skip_reason='' WHERE id=?1",
                [question_id],
            )?;
        }
        Some(false) => {
            conn.execute(
                "UPDATE questions SET status=CASE WHEN answer='' THEN 'open' ELSE 'draft' END,
                        skipped_by='', skip_reason='', restored=1
                 WHERE id=?1 AND status='skipped'",
                [question_id],
            )?;
        }
        None => {}
    }
    Ok(())
}

/// いまの議題を切り替える（#99）。前の議題には、いまの議題だった時間を積み上げる。
/// `at` は録音時間の秒。無ければ会議の録音の長さを使う。同じ議題なら何もしない。
pub fn set_active_question(conn: &Connection, meeting_id: i64, question: Option<i64>, at: Option<f64>) -> Result<()> {
    let (current, duration): (Option<i64>, f64) = conn.query_row(
        "SELECT active_question_id, duration_sec FROM meetings WHERE id=?1",
        [meeting_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if current == question {
        return Ok(());
    }
    let at = at.unwrap_or(duration).max(0.0);
    conn.execute(
        "UPDATE questions SET spent_sec = spent_sec + MAX(0, ?1 - active_since), active_since = NULL
         WHERE meeting_id=?2 AND active_since IS NOT NULL",
        params![at, meeting_id],
    )?;
    if let Some(id) = question {
        conn.execute(
            "UPDATE questions SET active_since=?1 WHERE id=?2 AND meeting_id=?3",
            params![at, id, meeting_id],
        )?;
    }
    conn.execute(
        "UPDATE meetings SET active_question_id=?1 WHERE id=?2",
        params![question, meeting_id],
    )?;
    Ok(())
}

/// 「完了して次へ」: いまの議題を完了（充足）にし、次の未完了の議題をいまの議題にする。
pub fn complete_and_next(conn: &Connection, meeting_id: i64, question_id: i64, at: Option<f64>) -> Result<Option<i64>> {
    let questions = list_questions(conn, meeting_id)?;
    let Some(index) = questions.iter().position(|q| q.id == question_id) else {
        anyhow::bail!("議題が見つかりません");
    };
    conn.execute(
        "UPDATE questions SET status='sufficient', locked=1 WHERE id=?1",
        [question_id],
    )?;
    let open = |q: &&Question| q.id != question_id && matches!(q.status.as_str(), "open" | "draft");
    // 後ろの議題を先に探し、無ければ前に残っている議題へ戻る。
    let next = questions[index + 1..]
        .iter()
        .find(open)
        .or_else(|| questions[..index].iter().find(open))
        .map(|q| q.id);
    set_active_question(conn, meeting_id, next, at)?;
    Ok(next)
}

/// 議題ごとの経過時間（秒）。いまの議題は、録音の長さまでを足す。
pub fn spent_seconds(question: &Question, duration_sec: f64) -> f64 {
    question.spent_sec + question.active_since.map(|since| (duration_sec - since).max(0.0)).unwrap_or(0.0)
}

/// 文の比べ方（空白・記号・「？」の違いを無視する）。
fn normalized(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

/// AI が提案した「足す・外す」を反映する。足すのは 1 回に 1 件・1 会議で 5 件まで、
/// 外すのは 1 回に 1 件まで。人が確認した・戻した質問は外さない。変わったら true。
pub fn ai_adjust_questions(
    conn: &Connection,
    meeting_id: i64,
    additions: &[String],
    skips: &[(i64, String)],
) -> Result<bool> {
    let current = list_questions(conn, meeting_id)?;
    let mut changed = false;
    let ai_added = current.iter().filter(|q| q.origin == "ai").count();
    if ai_added < 5 {
        let known: Vec<String> = current.iter().map(|q| normalized(&q.body)).collect();
        let candidate = additions.iter().map(|body| body.trim()).find(|body| {
            let key = normalized(body);
            let count = body.chars().count();
            (4..=80).contains(&count)
                && !key.is_empty()
                && !known.iter().any(|old| old == &key || old.contains(&key) || key.contains(old.as_str()))
        });
        if let Some(body) = candidate {
            let next: i64 = conn.query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM questions WHERE meeting_id=?1",
                [meeting_id],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT INTO questions (meeting_id, position, body, origin) VALUES (?1, ?2, ?3, 'ai')",
                params![meeting_id, next, body],
            )?;
            changed = true;
        }
    }
    for (question_id, reason) in skips.iter().take(1) {
        let n = conn.execute(
            "UPDATE questions SET status='skipped', skipped_by='ai', skip_reason=?1
             WHERE id=?2 AND meeting_id=?3 AND status='open' AND locked=0 AND restored=0",
            params![reason.chars().take(80).collect::<String>(), question_id, meeting_id],
        )?;
        changed |= n > 0;
    }
    Ok(changed)
}

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub position: i64,
    pub meetings: i64,
}

pub fn list_folders(conn: &Connection, user_id: i64) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.name, f.position, (SELECT COUNT(*) FROM meetings m WHERE m.folder_id = f.id)
         FROM folders f WHERE f.user_id=?1 ORDER BY f.position, f.id",
    )?;
    let rows = stmt.query_map([user_id], |row| {
        Ok(Folder {
            id: row.get(0)?,
            name: row.get(1)?,
            position: row.get(2)?,
            meetings: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn folder_owned(conn: &Connection, user_id: i64, folder_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM folders WHERE id=?1 AND user_id=?2",
            params![folder_id, user_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn create_folder(conn: &Connection, user_id: i64, name: &str) -> Result<i64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM folders WHERE user_id=?1", [user_id], |r| r.get(0))?;
    if count >= 100 {
        anyhow::bail!("フォルダは100個までです");
    }
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM folders WHERE user_id=?1",
        [user_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO folders (user_id, name, position, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![user_id, name, next, now_stamp()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_folder(conn: &Connection, user_id: i64, folder_id: i64, name: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE folders SET name=?1 WHERE id=?2 AND user_id=?3",
        params![name, folder_id, user_id],
    )? > 0)
}

/// フォルダを消しても、中の会議は消さずに「フォルダなし」へ戻す。
pub fn delete_folder(conn: &Connection, user_id: i64, folder_id: i64) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE meetings SET folder_id=NULL WHERE folder_id=?1 AND user_id=?2",
        params![folder_id, user_id],
    )?;
    let n = tx.execute("DELETE FROM folders WHERE id=?1 AND user_id=?2", params![folder_id, user_id])?;
    tx.commit()?;
    Ok(n > 0)
}

/// 会議をフォルダへ移す（None はフォルダなし）。フォルダは同じ会員のものに限る。
pub fn move_meeting(conn: &Connection, user_id: i64, meeting_id: i64, folder_id: Option<i64>) -> Result<bool> {
    if let Some(folder) = folder_id {
        if !folder_owned(conn, user_id, folder)? {
            anyhow::bail!("フォルダが見つかりません");
        }
    }
    Ok(conn.execute(
        "UPDATE meetings SET folder_id=?1 WHERE id=?2 AND user_id=?3",
        params![folder_id, meeting_id, user_id],
    )? > 0)
}

/// 画面で並べ替えた順に保存する。渡された会議の中だけで、元の並び順の枠を入れ替える
/// （絞り込んだ一部だけを並べ替えても、ほかの会議との前後は崩さない）。
pub fn reorder_meetings(conn: &Connection, user_id: i64, ids: &[i64]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let mut slots: Vec<i64> = Vec::new();
    for id in ids {
        let position: Option<i64> = tx
            .query_row(
                "SELECT position FROM meetings WHERE id=?1 AND user_id=?2",
                params![id, user_id],
                |r| r.get(0),
            )
            .optional()?;
        slots.push(position.context("会議が見つかりません")?);
    }
    slots.sort_unstable();
    slots.dedup();
    // 同じ値が重なっていたら、連番を振り直して前後を決められるようにする。
    if slots.len() < ids.len() {
        let base = slots.first().copied().unwrap_or(0);
        slots = (0..ids.len() as i64).map(|i| base + i).collect();
        let shift = ids.len() as i64;
        tx.execute(
            "UPDATE meetings SET position = position + ?1 WHERE user_id=?2 AND position >= ?3",
            params![shift, user_id, base],
        )?;
    }
    for (id, position) in ids.iter().zip(slots) {
        tx.execute(
            "UPDATE meetings SET position=?1 WHERE id=?2 AND user_id=?3",
            params![position, id, user_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    /// decision（決定）/ todo（宿題）/ number（数字・日付）/ concern（懸念）/ term（用語）
    pub kind: String,
    pub text: String,
    pub detail: String,
    pub start_sec: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct NoteDraft {
    pub kind: String,
    pub text: String,
    pub detail: String,
}

pub const NOTE_KINDS: [&str; 5] = ["decision", "todo", "number", "concern", "term"];

/// AI のメモを積む。1 回に 3 件まで。同じ種類・同じ内容（用語は同じ語）は重ねない。追加した件数を返す。
pub fn add_notes(conn: &Connection, meeting_id: i64, notes: &[NoteDraft], segment_id: Option<i64>) -> Result<usize> {
    let existing = list_notes(conn, meeting_id)?;
    let mut keys: Vec<(String, String)> = existing.iter().map(|n| (n.kind.clone(), normalized(&n.text))).collect();
    let mut added = 0;
    for note in notes {
        if added >= 3 {
            break;
        }
        let text: String = note.text.trim().chars().take(120).collect();
        let key = (note.kind.clone(), normalized(&text));
        if !NOTE_KINDS.contains(&note.kind.as_str()) || key.1.is_empty() || keys.contains(&key) {
            continue;
        }
        let terms = existing.iter().filter(|n| n.kind == "term").count();
        if note.kind == "term" && terms >= 30 {
            continue;
        }
        conn.execute(
            "INSERT INTO meeting_notes (meeting_id, kind, text, detail, segment_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                meeting_id,
                note.kind,
                text,
                note.detail.trim().chars().take(200).collect::<String>(),
                segment_id,
                now_stamp()
            ],
        )?;
        keys.push(key);
        added += 1;
    }
    Ok(added)
}

pub fn list_notes(conn: &Connection, meeting_id: i64) -> Result<Vec<Note>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.kind, n.text, n.detail, s.start_sec FROM meeting_notes n
         LEFT JOIN segments s ON s.id = n.segment_id
         WHERE n.meeting_id=?1 ORDER BY n.id",
    )?;
    let rows = stmt.query_map([meeting_id], |row| {
        Ok(Note {
            id: row.get(0)?,
            kind: row.get(1)?,
            text: row.get(2)?,
            detail: row.get(3)?,
            start_sec: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn delete_note(conn: &Connection, meeting_id: i64, note_id: i64) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM meeting_notes WHERE id=?1 AND meeting_id=?2",
        params![note_id, meeting_id],
    )? > 0)
}

pub fn set_next_hint(conn: &Connection, meeting_id: i64, hint: &str) -> Result<()> {
    conn.execute(
        "UPDATE meetings SET next_hint=?1 WHERE id=?2",
        params![hint.trim().chars().take(120).collect::<String>(), meeting_id],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Mark {
    pub id: i64,
    pub at_sec: f64,
}

/// 「ここ重要」の印。会議の録音時間の秒で持つ。1 会議 200 件まで。
pub fn add_mark(conn: &Connection, meeting_id: i64, at_sec: f64) -> Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM meeting_marks WHERE meeting_id=?1", [meeting_id], |r| r.get(0))?;
    if count >= 200 {
        anyhow::bail!("印は1会議200件までです");
    }
    conn.execute(
        "INSERT INTO meeting_marks (meeting_id, at_sec, created_at) VALUES (?1, ?2, ?3)",
        params![meeting_id, at_sec.max(0.0), now_stamp()],
    )?;
    Ok(())
}

pub fn list_marks(conn: &Connection, meeting_id: i64) -> Result<Vec<Mark>> {
    let mut stmt = conn.prepare("SELECT id, at_sec FROM meeting_marks WHERE meeting_id=?1 ORDER BY at_sec, id")?;
    let rows = stmt.query_map([meeting_id], |row| Ok(Mark { id: row.get(0)?, at_sec: row.get(1)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn delete_mark(conn: &Connection, meeting_id: i64, mark_id: i64) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM meeting_marks WHERE id=?1 AND meeting_id=?2",
        params![mark_id, meeting_id],
    )? > 0)
}

/// 印の時刻に話していた区間（印の 30 秒前から印の時刻まで）。
pub fn marked_segment(mark: &Mark, segment: &Segment) -> bool {
    segment.start_sec <= mark.at_sec && segment.start_sec >= mark.at_sec - 30.0
}

pub fn delete_question(conn: &Connection, meeting_id: i64, question_id: i64) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM questions WHERE id=?1 AND meeting_id=?2",
        params![question_id, meeting_id],
    )?;
    Ok(n > 0)
}

pub fn list_segments(conn: &Connection, meeting_id: i64) -> Result<Vec<Segment>> {
    let mut stmt = conn.prepare(
        "SELECT id, start_sec, text, speaker FROM segments WHERE meeting_id=?1 ORDER BY start_sec, id",
    )?;
    let rows = stmt.query_map([meeting_id], |row| {
        Ok(Segment {
            id: row.get(0)?,
            start_sec: row.get(1)?,
            text: row.get(2)?,
            speaker: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn segment_exists(conn: &Connection, meeting_id: i64, start_sec: f64) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT id FROM segments WHERE meeting_id=?1 AND start_sec BETWEEN ?2 AND ?3 LIMIT 1",
            params![meeting_id, start_sec - 0.05, start_sec + 0.05],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

pub fn update_segment(
    conn: &Connection,
    meeting_id: i64,
    segment_id: i64,
    text: &str,
    speaker: Option<&str>,
) -> Result<Segment> {
    let n = conn.execute(
        "UPDATE segments SET text=?1 WHERE id=?2 AND meeting_id=?3",
        params![text, segment_id, meeting_id],
    )?;
    if n == 0 {
        anyhow::bail!("書き起こしが見つかりません");
    }
    if let Some(speaker) = speaker {
        // 人が付けた話者。以後の推定で上書きしない（空にしたら推定に戻す）。
        let speaker: String = speaker.trim().chars().take(30).collect();
        conn.execute(
            "UPDATE segments SET speaker=?1, speaker_manual=?2 WHERE id=?3",
            params![speaker, !speaker.is_empty() as i64, segment_id],
        )?;
    }
    Ok(conn.query_row(
        "SELECT id, start_sec, text, speaker FROM segments WHERE id=?1",
        [segment_id],
        |row| {
            Ok(Segment {
                id: row.get(0)?,
                start_sec: row.get(1)?,
                text: row.get(2)?,
                speaker: row.get(3)?,
            })
        },
    )?)
}

pub fn list_hits(conn: &Connection, meeting_id: i64) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT id, source, title, locator, snippet, query, pinned FROM knowledge_hits WHERE meeting_id=?1 ORDER BY pinned DESC, id DESC",
    )?;
    let rows = stmt.query_map([meeting_id], |row| {
        Ok(Hit {
            id: row.get(0)?,
            source: row.get(1)?,
            title: row.get(2)?,
            locator: row.get(3)?,
            snippet: row.get(4)?,
            query: row.get(5)?,
            pinned: row.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn pinned_hits(conn: &Connection, meeting_id: i64) -> Result<Vec<Hit>> {
    Ok(list_hits(conn, meeting_id)?
        .into_iter()
        .filter(|hit| hit.pinned)
        .collect())
}

/// 打ち切った（もう自動では再試行しない）区間の失敗理由。
pub fn job_errors(conn: &Connection, meeting_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT error FROM audio_jobs WHERE meeting_id=?1 AND status='error' AND next_retry_at IS NULL AND error IS NOT NULL ORDER BY id DESC LIMIT 3",
    )?;
    let rows = stmt.query_map([meeting_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_live(conn: &Connection, meeting_id: i64) -> Result<()> {
    let status: String = meeting_status(conn, meeting_id)?;
    if status == "live" {
        return Ok(());
    }
    if status != "draft" {
        anyhow::bail!("この会議は開始できません");
    }
    conn.execute(
        "UPDATE meetings SET status='live', started_at=COALESCE(started_at, ?1), finish_requested=0 WHERE id=?2",
        params![now_stamp(), meeting_id],
    )?;
    Ok(())
}

/// 上限の確認つきの開始。同時録音数と今日の会議数をトランザクションの中で確かめる。
pub fn start_meeting_checked(
    conn: &mut Connection,
    user_id: i64,
    meeting_id: i64,
    limits: &crate::accounts::Limits,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let status = meeting_status(&tx, meeting_id)?;
    if status == "live" {
        return Ok(());
    }
    if status != "draft" {
        anyhow::bail!("この会議は開始できません");
    }
    crate::accounts::gate_start(&tx, user_id, meeting_id, limits)?;
    set_live(&tx, meeting_id)?;
    tx.execute(
        "UPDATE meetings SET last_audio_at=?1 WHERE id=?2",
        params![now_stamp(), meeting_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn resume_meeting_checked(
    conn: &mut Connection,
    user_id: i64,
    meeting_id: i64,
    limits: &crate::accounts::Limits,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    crate::accounts::gate_resume(&tx, user_id, meeting_id, limits)?;
    set_resumed(&tx, meeting_id)?;
    tx.execute(
        "UPDATE meetings SET last_audio_at=?1 WHERE id=?2",
        params![now_stamp(), meeting_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// 録音中のまま音声が長く届かない会議（タブを閉じた等）を一時停止にし、同時録音の枠を返す。
pub fn pause_stale(conn: &Connection, idle_minutes: i64) -> Result<Vec<i64>> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(idle_minutes))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut stmt = conn.prepare(
        "SELECT id FROM meetings WHERE status='live' AND finish_requested=0
           AND COALESCE(last_audio_at, started_at, created_at) < ?1",
    )?;
    let ids: Vec<i64> = stmt
        .query_map([&cutoff], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in &ids {
        conn.execute("UPDATE meetings SET status='paused' WHERE id=?1 AND status='live'", [id])?;
    }
    Ok(ids)
}

/// 運用者の操作。持ち主が未設定の会議・テンプレート・索引と、旧来の共通設定を指定の会員へ移す。
/// 何度実行しても同じ結果になる（既に持ち主がいるものは動かさない）。
pub fn assign_legacy(conn: &Connection, user_id: i64, data_dir: &Path) -> Result<(i64, i64, i64)> {
    let tx = conn.unchecked_transaction()?;
    // 設定（ナレッジフォルダを含む）を先に移す。後に移すと、フォルダの変更で索引が消える。
    let legacy = get_settings(&tx)?;
    // 会員がすでに入れた値は残し、空の項目だけを旧来の共通設定で埋める（提供元の選択も変えない）。
    let mine = get_user_settings(&tx, user_id)?;
    let fill = |current: &str, legacy: String| (current.is_empty() && !legacy.is_empty()).then_some(legacy);
    let default_prompt = mine.summary_prompt.trim().is_empty() || mine.summary_prompt == DEFAULT_SUMMARY_PROMPT;
    patch_user_settings_in(
        &tx,
        user_id,
        SettingsPatch {
            api_key: fill(&mine.api_key, legacy.api_key.clone()),
            model: mine.api_key.is_empty().then_some(legacy.model.clone()),
            summary_prompt: (default_prompt && legacy.summary_prompt != DEFAULT_SUMMARY_PROMPT)
                .then_some(legacy.summary_prompt.clone()),
            knowledge_path: fill(&mine.knowledge_path, legacy.knowledge_path.clone()),
            fess_url: fill(&mine.fess_url, legacy.fess_url.clone()),
            fess_token: fill(&mine.fess_token, legacy.fess_token.clone()),
            fess_enabled: (mine.fess_url.is_empty() && !legacy.fess_url.is_empty()).then_some(legacy.fess_enabled),
            ..Default::default()
        },
    )?;
    // 移し終えた共通設定のキーは残さない（会員の設定に暗号化して入れてある）。
    tx.execute("UPDATE settings SET api_key='', fess_token='' WHERE id=1", [])?;
    let meetings = tx.execute("UPDATE meetings SET user_id=?1 WHERE user_id IS NULL", [user_id])? as i64;
    let templates = tx.execute("UPDATE templates SET user_id=?1 WHERE user_id IS NULL", [user_id])? as i64;
    let docs = tx.execute("UPDATE knowledge_docs SET user_id=?1 WHERE user_id IS NULL", [user_id])? as i64;
    tx.execute("UPDATE knowledge_chunks SET user_id=?1 WHERE user_id IS NULL", [user_id])?;
    // 録音の大きさは版 4 より前は記録していないので、ファイルから測り直す。
    let mut stmt = tx.prepare(
        "SELECT j.id, j.meeting_id, j.wav_path FROM audio_jobs j JOIN meetings m ON m.id = j.meeting_id
         WHERE m.user_id=?1 AND j.bytes=0",
    )?;
    let rows: Vec<(i64, i64, String)> = stmt
        .query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for (job_id, meeting_id, stored) in rows {
        let path = resolve_audio_path(data_dir, meeting_id, &stored);
        if let Ok(meta) = std::fs::metadata(&path) {
            let bytes = meta.len() as i64;
            let duration_ms = (bytes.saturating_sub(44) / 2) * 1000 / 16_000;
            tx.execute(
                "UPDATE audio_jobs SET bytes=?1, duration_ms=?2 WHERE id=?3",
                params![bytes, duration_ms, job_id],
            )?;
        }
    }
    tx.commit()?;
    Ok((meetings, templates, docs))
}

/// 移行前のデータが残っているか（運用者への案内用）。
pub fn legacy_counts(conn: &Connection) -> Result<(i64, i64)> {
    let meetings: i64 =
        conn.query_row("SELECT COUNT(*) FROM meetings WHERE user_id IS NULL", [], |r| r.get(0))?;
    let templates: i64 =
        conn.query_row("SELECT COUNT(*) FROM templates WHERE user_id IS NULL", [], |r| r.get(0))?;
    Ok((meetings, templates))
}

pub fn set_paused(conn: &Connection, meeting_id: i64) -> Result<()> {
    let (status, finish) = meeting_state(conn, meeting_id)?;
    if status == "paused" && !finish {
        return Ok(());
    }
    if status != "live" || finish {
        anyhow::bail!("進行中の会議だけ一時停止できます");
    }
    conn.execute(
        "UPDATE meetings SET status='paused' WHERE id=?1",
        [meeting_id],
    )?;
    Ok(())
}

/// 再読み込み後のタブが録音を続けられるよう、live でも受け付ける。
pub fn set_resumed(conn: &Connection, meeting_id: i64) -> Result<()> {
    let (status, finish) = meeting_state(conn, meeting_id)?;
    if finish || (status != "paused" && status != "live") {
        anyhow::bail!("この会議は再開できません");
    }
    conn.execute(
        "UPDATE meetings SET status='live' WHERE id=?1",
        [meeting_id],
    )?;
    Ok(())
}

/// 録音中の文字起こしのしかたと、文字だけにするかを返す。
pub fn meeting_ears(conn: &Connection, meeting_id: i64) -> Result<(String, bool)> {
    conn.query_row(
        "SELECT speech, transcript_only FROM meetings WHERE id=?1",
        [meeting_id],
        |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
    )
    .optional()?
    .context("会議がありません")
}

pub fn finish_close(conn: &Connection, meeting_id: i64) -> Result<String> {
    conn.query_row("SELECT finish_close FROM meetings WHERE id=?1", [meeting_id], |r| r.get(0))
        .optional()?
        .context("会議がありません")
}

fn normalize_speech(speech: &str) -> Result<&str> {
    match speech {
        "api" | "chrome" | "off" => Ok(speech),
        _ => anyhow::bail!("文字起こしのしかたが正しくありません"),
    }
}

/// 録音中に、API・Chrome・録音だけのどれで文字にするかを切り替える。
/// API に戻したときは、保留していた録音を文字起こし待ちに戻す。
pub fn set_speech(conn: &Connection, meeting_id: i64, speech: &str, transcript_only: bool) -> Result<()> {
    let status = meeting_status(conn, meeting_id)?;
    if status != "draft" && status != "live" && status != "paused" {
        anyhow::bail!("この会議の文字起こしは切り替えられません");
    }
    let speech = normalize_speech(speech)?;
    conn.execute(
        "UPDATE meetings SET speech=?1, transcript_only=?2 WHERE id=?3",
        params![speech, transcript_only as i64, meeting_id],
    )?;
    if speech == "api" {
        promote_held(conn, meeting_id)?;
    }
    Ok(())
}

/// Chrome や録音だけの間に保留した音声を、API の文字起こし待ちにする。
pub fn promote_held(conn: &Connection, meeting_id: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE audio_jobs SET status='pending' WHERE meeting_id=?1 AND status='held'",
        [meeting_id],
    )?)
}

/// 終了のしかたを記録してから終了を依頼する。`api` のときは保留中の録音も文字起こし待ちにする。
pub fn prepare_finish(conn: &Connection, meeting_id: i64, close: &str) -> Result<()> {
    let close = match close {
        "" | "api" | "chrome" | "plain" => close,
        _ => anyhow::bail!("終了のしかたが正しくありません"),
    };
    if close == "api" {
        promote_held(conn, meeting_id)?;
    }
    conn.execute(
        "UPDATE meetings SET finish_close=?1 WHERE id=?2",
        params![close, meeting_id],
    )?;
    request_finish(conn, meeting_id)
}

pub fn request_finish(conn: &Connection, meeting_id: i64) -> Result<()> {
    let status = meeting_status(conn, meeting_id)?;
    if status == "done" || status == "wrapping" {
        return Ok(());
    }
    if status != "live" && status != "paused" {
        anyhow::bail!("開始前の会議は終了できません");
    }
    conn.execute(
        "UPDATE meetings SET finish_requested=1 WHERE id=?1",
        [meeting_id],
    )?;
    Ok(())
}

/// 終了を押したあと、要約に取りかかる前なら取り消して一時停止に戻す。
pub fn cancel_finish(conn: &Connection, meeting_id: i64) -> Result<()> {
    let (status, finish) = meeting_state(conn, meeting_id)?;
    if !finish || (status != "live" && status != "paused") {
        anyhow::bail!("要約に取りかかったため、録音には戻せません");
    }
    conn.execute(
        "UPDATE meetings SET finish_requested=0, status='paused' WHERE id=?1",
        [meeting_id],
    )?;
    Ok(())
}

fn meeting_status(conn: &Connection, meeting_id: i64) -> Result<String> {
    Ok(meeting_state(conn, meeting_id)?.0)
}

fn meeting_state(conn: &Connection, meeting_id: i64) -> Result<(String, bool)> {
    conn.query_row(
        "SELECT status, finish_requested FROM meetings WHERE id=?1",
        [meeting_id],
        |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
    )
    .optional()?
    .context("会議がありません")
}

pub fn meeting_title(conn: &Connection, meeting_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT title FROM meetings WHERE id=?1",
        [meeting_id],
        |r| r.get(0),
    )?)
}

/// 音声区間を待ち行列へ入れる。文字起こしはここではしない。
/// 無音の区間は録音として残し、文字起こしは済んだ扱いにする。
pub struct AudioChunk<'a> {
    pub start_ms: i64,
    pub wav_path: &'a str,
    pub duration_sec: f64,
    pub bytes: i64,
    pub silent: bool,
    /// ボイスレコーダーなどのファイルから取り込んだ区間。完了済みの会議にも入れられ、
    /// 既存の区間と重なる位置は断る（#96）。
    pub import: bool,
}

/// 取り込みを受け付ける会議の状態か。録音中・一時停止中・完了済み（まとめ中は不可）。
fn import_open(status: &str) -> bool {
    matches!(status, "live" | "paused" | "done")
}

/// 既存の区間と時間が重なるか（同じ開始時刻は重複送信なので除く）。
fn overlaps(conn: &Connection, meeting_id: i64, start_ms: i64, duration_ms: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM audio_jobs WHERE meeting_id=?1 AND start_ms<>?2
               AND start_ms < ?2 + ?3 AND start_ms + duration_ms > ?2 LIMIT 1",
            params![meeting_id, start_ms, duration_ms],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// 上限の確認つきで区間を登録する。録音時間と保存容量は、ここで受け付けた分から数える。
pub fn enqueue_audio_checked(
    conn: &mut Connection,
    user_id: i64,
    meeting_id: i64,
    chunk: &AudioChunk,
    limits: &crate::accounts::Limits,
) -> Result<(i64, bool)> {
    enqueue(conn, meeting_id, chunk, Some((user_id, limits)))
}

#[cfg(test)]
pub fn enqueue_audio(
    conn: &mut Connection,
    meeting_id: i64,
    start_ms: i64,
    wav_path: &str,
    duration_sec: f64,
    silent: bool,
) -> Result<(i64, bool)> {
    let chunk = AudioChunk {
        start_ms,
        wav_path,
        duration_sec,
        bytes: 0,
        silent,
        import: false,
    };
    enqueue(conn, meeting_id, &chunk, None)
}

fn enqueue(
    conn: &mut Connection,
    meeting_id: i64,
    chunk: &AudioChunk,
    gate: Option<(i64, &crate::accounts::Limits)>,
) -> Result<(i64, bool)> {
    let (start_ms, wav_path, duration_sec, silent) =
        (chunk.start_ms, chunk.wav_path, chunk.duration_sec, chunk.silent);
    let duration_ms = (duration_sec * 1000.0).round() as i64;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (status, finish, speech): (String, bool, String) = tx
        .query_row(
            "SELECT status, finish_requested, speech FROM meetings WHERE id=?1",
            [meeting_id],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?)),
        )
        .optional()?
        .context("会議がありません")?;
    if chunk.import {
        if !import_open(&status) {
            anyhow::bail!("この会議には取り込めません（まとめ中か下書きです）");
        }
    } else if finish || (status != "live" && status != "paused") {
        anyhow::bail!("この会議は録音を受け付けていません");
    }
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM audio_jobs WHERE meeting_id=?1 AND start_ms=?2",
            params![meeting_id, start_ms],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        tx.commit()?;
        return Ok((id, false));
    }
    if chunk.import && overlaps(&tx, meeting_id, start_ms, duration_ms)? {
        anyhow::bail!("取り込む位置が既存の録音と重なっています。「続きとして末尾に足す」を選ぶか、開始時刻を変えてください");
    }
    if let Some((user_id, limits)) = gate {
        crate::accounts::gate_audio(&tx, user_id, meeting_id, duration_ms, chunk.bytes, limits)?;
    }
    tx.execute(
        "INSERT INTO audio_jobs (meeting_id, start_ms, wav_path, status, created_at, silent, duration_ms, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            meeting_id,
            start_ms,
            wav_path,
            if silent {
                "done"
            } else if speech == "chrome" || speech == "off" {
                "held"
            } else {
                "pending"
            },
            now_stamp(),
            silent as i64,
            duration_ms,
            chunk.bytes
        ],
    )?;
    let id = tx.last_insert_rowid();
    let end = start_ms as f64 / 1000.0 + duration_sec;
    tx.execute(
        "UPDATE meetings SET duration_sec = MAX(duration_sec, ?1), last_audio_at=?2 WHERE id=?3",
        params![end, now_stamp(), meeting_id],
    )?;
    tx.commit()?;
    Ok((id, true))
}

pub fn claim_pending(conn: &mut Connection) -> Result<Option<AudioJob>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job = tx
        .query_row(
            "SELECT id, meeting_id, start_ms, wav_path FROM audio_jobs WHERE status='pending' ORDER BY id LIMIT 1",
            [],
            |row| {
                Ok(AudioJob {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    start_ms: row.get(2)?,
                    wav_path: row.get(3)?,
                })
            },
        )
        .optional()?;
    if let Some(job) = &job {
        tx.execute(
            "UPDATE audio_jobs SET status='processing' WHERE id=?1",
            [job.id],
        )?;
    }
    tx.commit()?;
    Ok(job)
}

/// 終了を押され、未処理も再試行待ちも残っていない会議を 1 件選び、まとめ中にする。
/// まとめ中の会議は選び直さない（要約の二重実行を防ぐ）。
pub fn claim_finish(conn: &mut Connection) -> Result<Option<i64>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut stmt = tx.prepare(
        "SELECT id FROM meetings WHERE finish_requested=1 AND status IN ('live','paused') ORDER BY id",
    )?;
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut chosen = None;
    for id in ids {
        if unfinished_jobs(&tx, id)? == 0 {
            tx.execute("UPDATE meetings SET status='wrapping' WHERE id=?1", [id])?;
            chosen = Some(id);
            break;
        }
    }
    tx.commit()?;
    Ok(chosen)
}

pub fn reset_inflight(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE audio_jobs SET status='pending' WHERE status='processing'",
        [],
    )?;
    conn.execute(
        "UPDATE meetings SET status='live' WHERE status='wrapping'",
        [],
    )?;
    Ok(())
}

pub fn mark_job_done(conn: &Connection, job_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE audio_jobs SET status='done', error=NULL, next_retry_at=NULL WHERE id=?1",
        [job_id],
    )?;
    Ok(())
}

/// 自動の再試行を打ち切る。回数を上限にして、再起動しても送り直さない。
pub fn mark_job_error(conn: &Connection, job_id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE audio_jobs SET status='error', error=?1, next_retry_at=NULL, attempts=MAX(attempts, ?2) WHERE id=?3",
        params![error, MAX_ATTEMPTS, job_id],
    )?;
    Ok(())
}

pub enum AudioSlot {
    Fresh,
    Duplicate(i64),
    Closed(String),
}

pub fn audio_slot(conn: &Connection, meeting_id: i64, start_ms: i64, import: bool) -> Result<AudioSlot> {
    let state = meeting_state(conn, meeting_id);
    let Ok((status, finish)) = state else {
        return Ok(AudioSlot::Closed("会議がありません".into()));
    };
    if import {
        if !import_open(&status) {
            return Ok(AudioSlot::Closed("この会議には取り込めません（まとめ中か下書きです）".into()));
        }
    } else if finish || (status != "live" && status != "paused") {
        return Ok(AudioSlot::Closed("この会議は録音を受け付けていません".into()));
    }
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM audio_jobs WHERE meeting_id=?1 AND start_ms=?2",
            params![meeting_id, start_ms],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(AudioSlot::Duplicate(id));
    }
    Ok(AudioSlot::Fresh)
}

pub fn stamp_after(secs: u64) -> String {
    let when = chrono::Utc::now() + chrono::Duration::seconds(secs as i64);
    when.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn job_attempts(conn: &Connection, job_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT attempts FROM audio_jobs WHERE id=?1",
        [job_id],
        |row| row.get(0),
    )?)
}

pub fn schedule_retry(conn: &Connection, job_id: i64, error: &str, when: &str) -> Result<()> {
    conn.execute(
        "UPDATE audio_jobs SET status='error', error=?1, attempts=attempts+1, next_retry_at=?2 WHERE id=?3",
        params![error, when, job_id],
    )?;
    Ok(())
}

pub fn claim_due_retry(conn: &mut Connection) -> Result<Option<AudioJob>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_stamp();
    let job = tx
        .query_row(
            "SELECT id, meeting_id, start_ms, wav_path FROM audio_jobs
             WHERE status='error' AND next_retry_at IS NOT NULL AND next_retry_at<=?1 AND attempts<?2
             ORDER BY next_retry_at, id LIMIT 1",
            params![now, MAX_ATTEMPTS],
            |row| {
                Ok(AudioJob {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    start_ms: row.get(2)?,
                    wav_path: row.get(3)?,
                })
            },
        )
        .optional()?;
    if let Some(job) = &job {
        tx.execute(
            "UPDATE audio_jobs SET status='processing', next_retry_at=NULL WHERE id=?1",
            [job.id],
        )?;
    }
    tx.commit()?;
    Ok(job)
}

/// 打ち切った（欠落として扱う）区間の数。再試行待ちは含めない。
pub fn count_error_jobs(conn: &Connection, meeting_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM audio_jobs WHERE meeting_id=?1 AND status='error' AND next_retry_at IS NULL",
        [meeting_id],
        |row| row.get(0),
    )?)
}

/// 画面の「再試行」。打ち切った区間も含め、回数を戻して今すぐ送り直す。
pub fn release_retries(conn: &Connection, meeting_id: i64) -> Result<i64> {
    let now = now_stamp();
    let n = conn.execute(
        "UPDATE audio_jobs SET next_retry_at=?1, attempts=0 WHERE meeting_id=?2 AND status='error'",
        params![now, meeting_id],
    )?;
    Ok(n as i64)
}

/// 再試行待ちをあきらめ、欠落として要約に進めるようにする。
pub fn give_up_retries(conn: &Connection, meeting_id: i64) -> Result<i64> {
    let n = conn.execute(
        "UPDATE audio_jobs SET next_retry_at=NULL, attempts=MAX(attempts, ?1)
         WHERE meeting_id=?2 AND status='error' AND next_retry_at IS NOT NULL",
        params![MAX_ATTEMPTS, meeting_id],
    )?;
    Ok(n as i64)
}

#[derive(Debug, Clone)]
pub struct ErrorJob {
    pub id: i64,
    pub meeting_id: i64,
    pub wav_path: String,
    pub error: String,
    pub attempts: i64,
}

/// 再試行の予約が無いまま残った失敗区間（版 2 より前の行）。
pub fn list_failed_jobs(conn: &Connection) -> Result<Vec<ErrorJob>> {
    let mut stmt = conn.prepare(
        "SELECT id, meeting_id, wav_path, COALESCE(error, ''), attempts FROM audio_jobs
         WHERE status='error' AND next_retry_at IS NULL AND attempts<?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([MAX_ATTEMPTS], |row| {
        Ok(ErrorJob {
            id: row.get(0)?,
            meeting_id: row.get(1)?,
            wav_path: row.get(2)?,
            error: row.get(3)?,
            attempts: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn arm_retry(conn: &Connection, job_id: i64, when: &str) -> Result<()> {
    conn.execute(
        "UPDATE audio_jobs SET next_retry_at=?1 WHERE id=?2 AND status='error' AND next_retry_at IS NULL",
        params![when, job_id],
    )?;
    Ok(())
}

pub fn release_finish_claim(conn: &Connection, meeting_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE meetings SET status='live' WHERE id=?1 AND status='wrapping'",
        [meeting_id],
    )?;
    Ok(())
}

/// 処理する区間より前の、直近 2 分の書き起こし。後ろの区間は混ぜない。
pub fn recent_transcript(conn: &Connection, meeting_id: i64, start_ms: i64) -> Result<String> {
    let from = (start_ms - 120_000).max(0) as f64 / 1000.0;
    let until = start_ms as f64 / 1000.0 - 0.05;
    let mut stmt = conn.prepare(
        "SELECT start_sec, text FROM segments WHERE meeting_id=?1 AND start_sec>=?2 AND start_sec<?3 ORDER BY start_sec",
    )?;
    let rows = stmt.query_map(params![meeting_id, from, until], |row| {
        Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut text = String::new();
    for row in rows {
        let (sec, line) = row?;
        let whole = sec.floor() as i64;
        text.push_str(&format!("[{:02}:{:02}] {line}\n", whole / 60, whole % 60));
    }
    Ok(tail_chars(&text, 6000))
}

/// 取り込んだ書き起こしを入れる（#96）。同じ時刻に区間があれば入れない。
/// 入れた区間（ID・時刻・文）を返し、会議の長さを最後の区間まで延ばす。
pub fn import_segments(conn: &Connection, meeting_id: i64, items: &[(f64, String)]) -> Result<Vec<Segment>> {
    let status = meeting_status(conn, meeting_id)?;
    if !matches!(status.as_str(), "live" | "paused" | "done") {
        anyhow::bail!("この会議には取り込めません（まとめ中か下書きです）");
    }
    let tx = conn.unchecked_transaction()?;
    let mut added = Vec::new();
    for (start_sec, text) in items {
        let text = text.trim();
        if text.is_empty() || !start_sec.is_finite() || *start_sec < 0.0 || segment_exists(&tx, meeting_id, *start_sec)? {
            continue;
        }
        let id = insert_segment(&tx, meeting_id, *start_sec, text)?;
        added.push(Segment { id, start_sec: *start_sec, text: text.to_string(), ..Default::default() });
    }
    if let Some(last) = added.iter().map(|s| s.start_sec).reduce(f64::max) {
        tx.execute(
            "UPDATE meetings SET duration_sec = MAX(duration_sec, ?1) WHERE id=?2",
            params![last + 5.0, meeting_id],
        )?;
    }
    tx.commit()?;
    Ok(added)
}

/// 終了後に推定した話者を入れる（#83）。人が付けた話者は変えない。入れた件数を返す。
pub fn set_auto_speakers(conn: &Connection, meeting_id: i64, speakers: &[(i64, String)]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut changed = 0;
    for (segment_id, speaker) in speakers {
        let speaker: String = speaker.trim().chars().take(30).collect();
        changed += tx.execute(
            "UPDATE segments SET speaker=?1 WHERE id=?2 AND meeting_id=?3 AND speaker_manual=0",
            params![speaker, segment_id, meeting_id],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Chrome の音声認識で確定した一文を、その会議の書き起こしとして残す。
/// 文字起こしが Chrome の会議で、録音中か一時停止中のときだけ受け付ける。
/// Chrome の確定文を受け取る。
/// - Chrome の音声認識の会議: 書き起こしとして保存する。終了後に遅れて届いた分も捨てずに足す。
/// - API で文字起こしする会議: 控え（caption_backup）にだけ残す。API が打ち切りになった区間の穴埋めに使う。
/// - 録音だけの会議: 控えに残す（あとで API に切り替えて失敗しても、何も無いよりよい）。
pub fn save_caption(conn: &Connection, meeting_id: i64, start_sec: f64, text: &str) -> Result<Option<Segment>> {
    let (status, speech): (String, String) = conn
        .query_row(
            "SELECT status, speech FROM meetings WHERE id=?1",
            [meeting_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("会議がありません")?;
    if !matches!(status.as_str(), "live" | "paused" | "wrapping" | "done") {
        anyhow::bail!("開始前の会議には書き起こしを保存できません");
    }
    let text: String = text.trim().chars().take(4_000).collect();
    if text.is_empty() {
        return Ok(None);
    }
    if !start_sec.is_finite() || start_sec < 0.0 || start_sec > 86_400.0 * 12.0 {
        anyhow::bail!("書き起こしの時刻が正しくありません");
    }
    if speech != "chrome" {
        conn.execute(
            "INSERT OR IGNORE INTO caption_backup (meeting_id, start_sec, text, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![meeting_id, start_sec, text, now_stamp()],
        )?;
        return Ok(None);
    }
    if segment_exists(conn, meeting_id, start_sec)? {
        return Ok(None);
    }
    let id = insert_segment(conn, meeting_id, start_sec, &text)?;
    Ok(Some(Segment { id, start_sec, text, ..Default::default() }))
}

/// 文字起こしが打ち切りになった区間（status='error' で再試行の予定が無いもの）に、
/// 速報字幕の控えを書き起こしとして入れる。入れた区間の数を返す。何度呼んでも同じ結果になる。
pub fn fill_gaps_from_backup(conn: &Connection, meeting_id: i64) -> Result<i64> {
    let gaps: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT start_ms, duration_ms FROM audio_jobs
             WHERE meeting_id=?1 AND status='error' AND next_retry_at IS NULL AND silent=0
             ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([meeting_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut filled = 0;
    for (start_ms, duration_ms) in gaps {
        let from = start_ms as f64 / 1000.0;
        // 区間の長さが記録されていない古い行は、録音区間の最長（15 秒）として扱う。
        let to = from + if duration_ms > 0 { duration_ms as f64 / 1000.0 } else { 15.0 };
        let lines: Vec<(f64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT start_sec, text FROM caption_backup
                 WHERE meeting_id=?1 AND start_sec>=?2 AND start_sec<?3 ORDER BY start_sec",
            )?;
            let rows = stmt.query_map(params![meeting_id, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut used = false;
        for (at, text) in lines {
            if !segment_exists(conn, meeting_id, at)? {
                insert_segment(conn, meeting_id, at, &format!("（速報）{text}"))?;
                used = true;
            }
        }
        if used {
            filled += 1;
        }
    }
    Ok(filled)
}

pub fn insert_segment(
    conn: &Connection,
    meeting_id: i64,
    start_sec: f64,
    text: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO segments (meeting_id, start_sec, text) VALUES (?1, ?2, ?3)",
        params![meeting_id, start_sec, text],
    )?;
    Ok(conn.last_insert_rowid())
}

fn status_rank(status: &str) -> i32 {
    match status {
        "sufficient" => 2,
        "draft" => 1,
        _ => 0,
    }
}

/// 回答候補を反映する。状態は open → draft → sufficient の向きにしか進めない。
/// 人が直した質問（locked）は触らない。
pub fn apply_answers(
    conn: &Connection,
    meeting_id: i64,
    active_question_id: Option<i64>,
    updates: &[AnswerUpdate],
    segment_id: Option<i64>,
) -> Result<Vec<Question>> {
    let current = list_questions(conn, meeting_id)?;
    for update in updates {
        let Some(question) = current.iter().find(|q| q.id == update.question_id) else {
            continue;
        };
        if question.locked || question.status == "skipped" {
            continue;
        }
        let candidate = update.candidate.trim();
        let proposed = match update.status.as_str() {
            "sufficient" => "sufficient",
            "draft" => "draft",
            _ if !candidate.is_empty() => "draft",
            _ => "open",
        };
        let status = if status_rank(proposed) > status_rank(&question.status) {
            proposed
        } else {
            question.status.as_str()
        };
        let answer = if candidate.is_empty() {
            question.answer.clone()
        } else {
            candidate.to_string()
        };
        // 根拠は区間ごとに積み重ねる。複数の発言から作った回答でも、前の根拠を消さない。
        let quote = update.evidence.trim();
        if let (Some(segment_id), false) = (segment_id, quote.is_empty() && candidate.is_empty()) {
            let quote: String = if quote.is_empty() { candidate } else { quote }.chars().take(200).collect();
            conn.execute(
                "INSERT INTO question_evidence (question_id, segment_id, quote, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![update.question_id, segment_id, quote, now_stamp()],
            )?;
        }
        if status == question.status && answer == question.answer {
            continue;
        }
        conn.execute(
            "UPDATE questions SET status=?1, answer=?2, source='ai' WHERE id=?3 AND meeting_id=?4",
            params![status, answer, update.question_id, meeting_id],
        )?;
    }
    if let Some(active) = active_question_id {
        // 扱わない議題と、完了した議題には戻さない（人が「完了して次へ」で進めた進行を AI が巻き戻さない）。
        let belongs = current.iter().any(|q| q.id == active && q.status != "skipped" && q.status != "sufficient");
        if belongs {
            // 経過時間は、話題が変わった区間の開始時刻で切り替える。
            let at = match segment_id {
                Some(id) => conn
                    .query_row("SELECT start_sec FROM segments WHERE id=?1", [id], |r| r.get(0))
                    .optional()?,
                None => None,
            };
            set_active_question(conn, meeting_id, Some(active), at)?;
        }
    }
    list_questions(conn, meeting_id)
}

pub fn store_hits(
    conn: &Connection,
    meeting_id: i64,
    hits: &[crate::knowledge::HitDraft],
) -> Result<()> {
    for hit in hits {
        let existing: Option<(i64, i64)> = conn
            .query_row(
                "SELECT id, pinned FROM knowledge_hits WHERE meeting_id=?1 AND source=?2 AND locator=?3",
                params![meeting_id, hit.source, hit.locator],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, pinned)) = existing {
            if pinned == 0 {
                conn.execute(
                    "UPDATE knowledge_hits SET title=?1, snippet=?2, query=?3 WHERE id=?4",
                    params![hit.title, hit.snippet, hit.query, id],
                )?;
            }
        } else {
            conn.execute(
                "INSERT INTO knowledge_hits (meeting_id, source, title, locator, snippet, query, pinned, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
                params![
                    meeting_id,
                    hit.source,
                    hit.title,
                    hit.locator,
                    hit.snippet,
                    hit.query,
                    now_stamp()
                ],
            )?;
        }
    }
    conn.execute(
        "DELETE FROM knowledge_hits WHERE meeting_id=?1 AND pinned=0 AND id NOT IN (
            SELECT id FROM knowledge_hits WHERE meeting_id=?1 AND pinned=0 ORDER BY id DESC LIMIT 40
         )",
        [meeting_id],
    )?;
    Ok(())
}

pub fn set_pin(conn: &Connection, meeting_id: i64, hit_id: i64, pinned: bool) -> Result<Hit> {
    let n = conn.execute(
        "UPDATE knowledge_hits SET pinned=?1 WHERE id=?2 AND meeting_id=?3",
        params![pinned as i64, hit_id, meeting_id],
    )?;
    if n == 0 {
        anyhow::bail!("ナレッジが見つかりません");
    }
    let hit = list_hits(conn, meeting_id)?
        .into_iter()
        .find(|hit| hit.id == hit_id)
        .context("ナレッジが見つかりません")?;
    Ok(hit)
}

pub fn resolve_audio_path(data_dir: &Path, meeting_id: i64, stored: &str) -> std::path::PathBuf {
    let stored_path = Path::new(stored);
    if stored_path.is_file() {
        return stored_path.to_path_buf();
    }
    let name = stored_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("chunk.wav"));
    data_dir
        .join("audio")
        .join(meeting_id.to_string())
        .join(name)
}

pub fn meeting_status_of(conn: &Connection, meeting_id: i64) -> Result<String> {
    meeting_status(conn, meeting_id)
}

pub fn chunk_paths(conn: &Connection, meeting_id: i64) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT wav_path FROM audio_jobs WHERE meeting_id=?1 ORDER BY start_ms, id")?;
    let rows = stmt.query_map([meeting_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// 要約を保存して完了にする。終了日時は最初に完了したときのまま残す。
pub fn save_summary(
    conn: &Connection,
    meeting_id: i64,
    summary: &str,
    error: Option<&str>,
) -> Result<()> {
    let now = now_stamp();
    conn.execute(
        "UPDATE meetings SET status='done', finish_requested=0, ended_at=COALESCE(ended_at, ?1), summary_at=?1, summary_md=?2, error=?3 WHERE id=?4",
        params![now, summary, error, meeting_id],
    )?;
    Ok(())
}

/// 人が直した要約。自動の失敗表示は消す。
pub fn update_summary(conn: &Connection, meeting_id: i64, summary: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE meetings SET summary_md=?1, summary_at=?2, error=NULL WHERE id=?3",
        params![summary, now_stamp(), meeting_id],
    )?;
    if n == 0 {
        anyhow::bail!("会議がありません");
    }
    Ok(())
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    text.chars().skip(count - max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrate(&conn).unwrap();
        crate::accounts::upsert_free_plan(&conn, &crate::accounts::free_limits_from_env()).unwrap();
        conn
    }

    fn live_meeting(conn: &mut Connection) -> i64 {
        let hash = crate::accounts::hash_password("password-for-test").unwrap();
        let user = match crate::accounts::create_user(conn, "tester", &hash).unwrap() {
            Some(user) => user,
            None => crate::accounts::user_id_by_login(conn, "tester").unwrap().unwrap(),
        };
        let id = create_meeting(conn, user, "会議", &["予算は？".into(), "期限は？".into()]).unwrap();
        set_live(conn, id).unwrap();
        id
    }

    #[test]
    fn ai_adds_and_skips_questions_within_limits() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let qs = list_questions(&conn, id).unwrap();
        // 既存とほぼ同じものは足さない。新しいものは 1 件だけ足す。
        assert!(!ai_adjust_questions(&conn, id, &["予算は?".into()], &[]).unwrap());
        assert!(ai_adjust_questions(&conn, id, &["決裁者は誰か".into(), "導入時期".into()], &[]).unwrap());
        let after = list_questions(&conn, id).unwrap();
        assert_eq!(after.len(), 3);
        assert_eq!(after[2].origin, "ai");
        // 外す: 未着手で、人が確認・戻していないものだけ。
        assert!(ai_adjust_questions(&conn, id, &[], &[(qs[0].id, "対象外と言われた".into())]).unwrap());
        let q0 = &list_questions(&conn, id).unwrap()[0];
        assert_eq!((q0.status.as_str(), q0.skipped_by.as_str(), q0.skip_reason.as_str()), ("skipped", "ai", "対象外と言われた"));
        // 外したものには AI の回答を書かない。
        let updated = apply_answers(&conn, id, None, &[AnswerUpdate { question_id: qs[0].id, candidate: "x".into(), status: "draft".into(), evidence: String::new() }], None).unwrap();
        assert_eq!(updated[0].answer, "");
        // 人が戻したら、AI は二度と外さない。
        let edit = |skip| QuestionEdit { body: None, answer: None, status: None, confirmed: None, skip: Some(skip), minutes: None };
        edit_question(&conn, id, qs[0].id, &edit(false)).unwrap();
        assert!(!ai_adjust_questions(&conn, id, &[], &[(qs[0].id, "再び".into())]).unwrap());
        assert_eq!(list_questions(&conn, id).unwrap()[0].status, "open");
        // 人の「今回は聞かない」。
        edit_question(&conn, id, qs[1].id, &edit(true)).unwrap();
        assert_eq!(list_questions(&conn, id).unwrap()[1].skipped_by, "human");
        // AI が足すのは 1 会議で 5 件まで。
        for n in 0..10 {
            ai_adjust_questions(&conn, id, &[format!("追加の確認事項その{n}")], &[]).unwrap();
        }
        assert_eq!(list_questions(&conn, id).unwrap().iter().filter(|q| q.origin == "ai").count(), 5);
    }

    #[test]
    fn imported_audio_and_text_go_into_done_meetings_without_overlap() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        enqueue_audio(&mut conn, id, 0, "/tmp/0.wav", 15.0, false).unwrap();
        conn.execute("UPDATE meetings SET status='done', finish_requested=1 WHERE id=?1", [id]).unwrap();
        let chunk = |start_ms: i64, import: bool| AudioChunk { start_ms, wav_path: "/tmp/x.wav", duration_sec: 10.0, bytes: 10, silent: false, import };
        // ライブの録音としては受け付けない。取り込みなら受け付ける。
        assert!(enqueue(&mut conn, id, &chunk(20_000, false), None).is_err());
        assert!(enqueue(&mut conn, id, &chunk(20_000, true), None).unwrap().1);
        // 既存の区間と重なる位置は断る。
        let err = enqueue(&mut conn, id, &chunk(10_000, true), None).unwrap_err().to_string();
        assert!(err.contains("重なって"), "{err}");
        assert!(matches!(audio_slot(&conn, id, 30_000, true).unwrap(), AudioSlot::Fresh));
        assert!(matches!(audio_slot(&conn, id, 30_000, false).unwrap(), AudioSlot::Closed(_)));
        // テキストの取り込み: 同じ時刻の区間は入れない。会議の長さを延ばす。
        insert_segment(&conn, id, 0.0, "既存").unwrap();
        let added = import_segments(&conn, id, &[(0.0, "重複".into()), (100.0, "取り込んだ発言".into()), (110.0, " ".into())]).unwrap();
        assert_eq!(added.len(), 1);
        assert!(get_meeting(&conn, id).unwrap().unwrap().duration_sec >= 105.0);
        // まとめ中には取り込めない。
        conn.execute("UPDATE meetings SET status='wrapping' WHERE id=?1", [id]).unwrap();
        assert!(import_segments(&conn, id, &[(200.0, "x".into())]).is_err());
    }

    #[test]
    fn chrome_speech_holds_audio_until_promoted() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        set_speech(&conn, id, "chrome", false).unwrap();
        let (job, fresh) = enqueue_audio(&mut conn, id, 0, "/tmp/0.wav", 15.0, false).unwrap();
        assert!(fresh);
        let status: String = conn.query_row("SELECT status FROM audio_jobs WHERE id=?1", [job], |r| r.get(0)).unwrap();
        assert_eq!(status, "held");
        assert_eq!(unfinished_jobs(&conn, id).unwrap(), 0, "保留は終了を待たせない");
        let saved = save_caption(&conn, id, 1.2, " こんにちは ").unwrap();
        assert_eq!(saved.unwrap().text, "こんにちは");
        assert!(save_caption(&conn, id, 1.2, "重複").unwrap().is_none());
        assert_eq!(promote_held(&conn, id).unwrap(), 1);
        let status: String = conn.query_row("SELECT status FROM audio_jobs WHERE id=?1", [job], |r| r.get(0)).unwrap();
        assert_eq!(status, "pending");
        set_speech(&conn, id, "off", false).unwrap();
        // 録るだけの会議でも、速報は控えとして残す（書き起こしにはしない）
        assert!(save_caption(&conn, id, 3.0, "録るだけ").unwrap().is_none());
        assert_eq!(list_segments(&conn, id).unwrap().len(), 1);
    }

    #[test]
    fn customer_docs_are_searched_only_within_their_meeting() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let user = crate::accounts::user_id_by_login(&conn, "tester").unwrap().unwrap();
        let other = create_meeting(&conn, user, "別の会議", &[]).unwrap();
        let doc = add_meeting_doc(&conn, id, "仕様書.docx", "docx", 100, &[("配管".into(), "配管の更新は来年度に予定しています".into())], None).unwrap();
        add_meeting_doc(&conn, other, "他社.pdf", "pdf", 100, &[(String::new(), "配管の別件".into())], None).unwrap();
        let hits = search_meeting_docs(&conn, id, &["配管".into()]).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "customer");
        assert!(hits[0].title.contains("仕様書.docx"));
        assert_eq!(meeting_doc_file(&conn, id, doc).unwrap().unwrap().0, format!("{doc}.docx"));
        assert!(meeting_doc_file(&conn, other, doc).unwrap().is_none(), "別の会議からは取り出せない");
        assert!(delete_meeting_doc(&conn, id, doc).unwrap());
        assert!(search_meeting_docs(&conn, id, &["配管".into()]).unwrap().is_empty());
    }

    #[test]
    fn agenda_tracks_time_per_item_and_moves_next() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let qs = list_questions(&conn, id).unwrap();
        set_agenda_minutes(&conn, id, &[Some(10), None]).unwrap();
        set_active_question(&conn, id, Some(qs[0].id), Some(0.0)).unwrap();
        // 120 秒で次の議題へ: 1 件目に 120 秒が積まれる。
        let next = complete_and_next(&conn, id, qs[0].id, Some(120.0)).unwrap();
        assert_eq!(next, Some(qs[1].id));
        let after = list_questions(&conn, id).unwrap();
        assert_eq!(after[0].status, "sufficient");
        assert!((after[0].spent_sec - 120.0).abs() < 1e-6);
        assert_eq!(after[0].minutes, Some(10));
        assert_eq!(after[1].active_since, Some(120.0));
        // いまの議題の経過は、録音の長さまでを足して見る。
        assert!((spent_seconds(&after[1], 300.0) - 180.0).abs() < 1e-6);
        // 同じ議題をもう一度指定しても時間は変わらない。
        set_active_question(&conn, id, Some(qs[1].id), Some(200.0)).unwrap();
        assert_eq!(list_questions(&conn, id).unwrap()[1].active_since, Some(120.0));
        // 最後の議題を完了すると、いまの議題は無くなる。
        assert_eq!(complete_and_next(&conn, id, qs[1].id, Some(240.0)).unwrap(), None);
        let end = list_questions(&conn, id).unwrap();
        assert!((end[1].spent_sec - 120.0).abs() < 1e-6);
        assert!(get_meeting(&conn, id).unwrap().unwrap().active_question_id.is_none());
        // AI の話題判定は、完了した議題をいまの議題に戻さない。
        apply_answers(&conn, id, Some(qs[0].id), &[], None).unwrap();
        assert!(get_meeting(&conn, id).unwrap().unwrap().active_question_id.is_none());
        // 件数の上限は無い（#98）。
        for n in 0..40 {
            add_question(&conn, id, &format!("議題{n}")).unwrap();
        }
        assert_eq!(list_questions(&conn, id).unwrap().len(), 42);
    }

    #[test]
    fn inferred_speakers_do_not_override_manual_ones() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let first = insert_segment(&conn, id, 0.0, "田中です。よろしくお願いします").unwrap();
        let second = insert_segment(&conn, id, 5.0, "佐藤です").unwrap();
        update_segment(&conn, id, second, "佐藤です", Some("佐藤")).unwrap();
        let changed = set_auto_speakers(&conn, id, &[(first, "田中".into()), (second, "話者A".into())]).unwrap();
        assert_eq!(changed, 1, "人が付けた話者は変えない");
        let segments = list_segments(&conn, id).unwrap();
        assert_eq!((segments[0].speaker.as_str(), segments[1].speaker.as_str()), ("田中", "佐藤"));
        // 人が空にしたら、推定に戻す。
        update_segment(&conn, id, second, "佐藤です", Some("")).unwrap();
        set_auto_speakers(&conn, id, &[(second, "話者B".into())]).unwrap();
        assert_eq!(list_segments(&conn, id).unwrap()[1].speaker, "話者B");
    }

    #[test]
    fn notes_are_deduplicated_and_capped() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let note = |kind: &str, text: &str| NoteDraft { kind: kind.into(), text: text.into(), detail: String::new() };
        let added = add_notes(
            &conn,
            id,
            &[note("decision", "来月から試行"), note("decision", "来月から試行。"), note("bogus", "x"), note("todo", "田中: 見積 金曜まで"), note("number", "予算 300万円"), note("concern", "人手不足")],
            None,
        )
        .unwrap();
        assert_eq!(added, 3, "同じ内容と不明な種類は除き、1 回に 3 件まで");
        let notes = list_notes(&conn, id).unwrap();
        assert_eq!(notes.iter().map(|n| n.kind.as_str()).collect::<Vec<_>>(), ["decision", "todo", "number"]);
        assert!(delete_note(&conn, id, notes[0].id).unwrap());
        assert_eq!(list_notes(&conn, id).unwrap().len(), 2);
    }

    #[test]
    fn folders_order_and_move_are_per_user() {
        let mut conn = memory();
        let first = live_meeting(&mut conn);
        let user = crate::accounts::user_id_by_login(&conn, "tester").unwrap().unwrap();
        let second = create_meeting(&conn, user, "二件目", &[]).unwrap();
        let third = create_meeting(&conn, user, "三件目", &[]).unwrap();
        let all = MeetingFilter { query: "", status: "", folder: "" };
        let ids = |conn: &Connection, filter: &MeetingFilter| list_meetings(conn, user, filter).unwrap().2.iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(&conn, &all), [third, second, first], "新しい会議は上に置く");
        reorder_meetings(&conn, user, &[first, third, second]).unwrap();
        assert_eq!(ids(&conn, &all), [first, third, second]);
        let folder = create_folder(&conn, user, "A社").unwrap();
        move_meeting(&conn, user, third, Some(folder)).unwrap();
        let text = folder.to_string();
        assert_eq!(ids(&conn, &MeetingFilter { query: "", status: "", folder: &text }), [third]);
        assert_eq!(ids(&conn, &MeetingFilter { query: "", status: "", folder: "none" }), [first, second]);
        // 別の会員のフォルダには移せない。
        let hash = crate::accounts::hash_password("password-for-test").unwrap();
        let other = crate::accounts::create_user(&conn, "other", &hash).unwrap().unwrap();
        let theirs = create_folder(&conn, other, "他人").unwrap();
        assert!(move_meeting(&conn, user, first, Some(theirs)).is_err());
        // フォルダを消しても会議は残り、フォルダなしに戻る。
        assert!(delete_folder(&conn, user, folder).unwrap());
        assert_eq!(ids(&conn, &MeetingFilter { query: "", status: "", folder: "none" }).len(), 3);
    }

    #[test]
    fn answer_status_never_moves_back() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let q = list_questions(&conn, id).unwrap()[0].id;
        apply_answers(
            &conn,
            id,
            None,
            &[AnswerUpdate {
                question_id: q,
                candidate: "増額".into(),
                status: "draft".into(),
                evidence: String::new(),
            }],
            None,
        )
        .unwrap();
        let after = apply_answers(
            &conn,
            id,
            None,
            &[AnswerUpdate {
                question_id: q,
                candidate: String::new(),
                status: "open".into(),
                evidence: String::new(),
            }],
            None,
        )
        .unwrap();
        assert_eq!(after[0].status, "draft");
        assert_eq!(after[0].answer, "増額");
    }

    #[test]
    fn edited_question_is_not_overwritten() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let q = list_questions(&conn, id).unwrap()[0].id;
        edit_question(
            &conn,
            id,
            q,
            &QuestionEdit {
                body: None,
                answer: Some("人が書いた答え"),
                status: Some("sufficient"),
                confirmed: None,
                skip: None,
                minutes: None,
            },
        )
        .unwrap();
        let after = apply_answers(
            &conn,
            id,
            None,
            &[AnswerUpdate {
                question_id: q,
                candidate: "AIの答え".into(),
                status: "draft".into(),
                evidence: String::new(),
            }],
            None,
        )
        .unwrap();
        assert_eq!(after[0].answer, "人が書いた答え");
        assert_eq!(after[0].status, "sufficient");
    }

    #[test]
    fn retry_waiting_blocks_finish_and_is_not_missing() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let (job, _) = enqueue_audio(&mut conn, id, 0, "/tmp/0.wav", 8.0, false).unwrap();
        claim_pending(&mut conn).unwrap();
        schedule_retry(&conn, job, "429", &stamp_after(3600)).unwrap();
        request_finish(&conn, id).unwrap();
        assert_eq!(claim_finish(&mut conn).unwrap(), None);
        let meeting = get_meeting(&conn, id).unwrap().unwrap();
        assert_eq!(meeting.retry_waiting, 1);
        assert_eq!(meeting.failed_jobs, 0);
        give_up_retries(&conn, id).unwrap();
        assert_eq!(claim_finish(&mut conn).unwrap(), Some(id));
        assert_eq!(count_error_jobs(&conn, id).unwrap(), 1);
        // 打ち切った区間は、起動時の再武装の対象にならない
        assert!(list_failed_jobs(&conn).unwrap().is_empty());
    }

    #[test]
    fn caption_backup_fills_only_given_up_chunks() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        conn.execute("UPDATE meetings SET speech='api' WHERE id=?1", [id]).unwrap();
        // API の会議では、速報は書き起こしにせず控えにだけ残す
        assert!(save_caption(&conn, id, 1.0, "最初の区間の速報").unwrap().is_none());
        assert!(save_caption(&conn, id, 9.0, "二つ目の区間の速報").unwrap().is_none());
        assert!(save_caption(&conn, id, 9.0, "同じ時刻の重複").unwrap().is_none());
        assert!(list_segments(&conn, id).unwrap().is_empty());
        let (failed, _) = enqueue_audio(&mut conn, id, 0, "/tmp/0.wav", 8.0, false).unwrap();
        let (waiting, _) = enqueue_audio(&mut conn, id, 8_000, "/tmp/8000.wav", 8.0, false).unwrap();
        mark_job_error(&conn, failed, "500").unwrap();
        schedule_retry(&conn, waiting, "429", &stamp_after(3600)).unwrap();
        // 再試行待ちの区間はまだ埋めない
        assert_eq!(fill_gaps_from_backup(&conn, id).unwrap(), 1);
        let segments = list_segments(&conn, id).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "（速報）最初の区間の速報");
        // 何度呼んでも重ならない
        assert_eq!(fill_gaps_from_backup(&conn, id).unwrap(), 0);
        give_up_retries(&conn, id).unwrap();
        assert_eq!(fill_gaps_from_backup(&conn, id).unwrap(), 1);
        assert_eq!(list_segments(&conn, id).unwrap().len(), 2);
    }

    #[test]
    fn chrome_caption_arriving_after_finish_is_kept() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        conn.execute("UPDATE meetings SET speech='chrome' WHERE id=?1", [id]).unwrap();
        request_finish(&conn, id).unwrap();
        conn.execute("UPDATE meetings SET status='done' WHERE id=?1", [id]).unwrap();
        let saved = save_caption(&conn, id, 3.0, "遅れて届いた確定文").unwrap();
        assert!(saved.is_some());
        assert!(save_caption(&conn, id, 3.0, "遅れて届いた確定文").unwrap().is_none());
    }

    #[test]
    fn final_error_is_not_rearmed_after_restart() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        let (job, _) = enqueue_audio(&mut conn, id, 0, "/tmp/0.wav", 8.0, false).unwrap();
        mark_job_error(&conn, job, "429 quota").unwrap();
        assert!(list_failed_jobs(&conn).unwrap().is_empty());
        assert_eq!(job_attempts(&conn, job).unwrap(), MAX_ATTEMPTS);
    }

    #[test]
    fn recent_transcript_excludes_later_segments() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        insert_segment(&conn, id, 0.0, "前の発言").unwrap();
        insert_segment(&conn, id, 16.0, "後ろの発言").unwrap();
        let text = recent_transcript(&conn, id, 8_000).unwrap();
        assert!(text.contains("前の発言"));
        assert!(!text.contains("後ろの発言"));
    }

    #[test]
    fn silent_chunk_is_kept_but_not_queued() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        enqueue_audio(&mut conn, id, 0, "/tmp/0.wav", 8.0, true).unwrap();
        assert_eq!(pending_jobs(&conn, id).unwrap(), 0);
        assert_eq!(chunk_paths(&conn, id).unwrap().len(), 1);
        assert!((get_meeting(&conn, id).unwrap().unwrap().duration_sec - 8.0).abs() < 1e-9);
    }

    #[test]
    fn resummary_keeps_first_end_time() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        conn.execute(
            "UPDATE meetings SET ended_at='2026-01-01T00:00:00Z' WHERE id=?1",
            [id],
        )
        .unwrap();
        save_summary(&conn, id, "要約", None).unwrap();
        let meeting = get_meeting(&conn, id).unwrap().unwrap();
        assert_eq!(meeting.ended_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(meeting.summary_at.is_some());
    }

    #[test]
    fn deleting_meeting_cascades_and_migration_adds_keys() {
        let mut conn = memory();
        let id = live_meeting(&mut conn);
        insert_segment(&conn, id, 0.0, "発言").unwrap();
        conn.execute("DELETE FROM meetings WHERE id=?1", [id]).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
        assert!(has_foreign_key(&conn, "audio_jobs").unwrap());
    }

    #[test]
    fn user_secrets_are_bound_to_member_and_field() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for (id, login) in [(1, "member1"), (2, "member2")] {
            conn.execute(
                "INSERT INTO users (id, login_id, password_hash, created_at) VALUES (?1, ?2, 'x', 'x')",
                params![id, login],
            )
            .unwrap();
        }
        patch_user_settings(&conn, 1, SettingsPatch { api_key: Some("key-one".into()), ..Default::default() }).unwrap();
        patch_user_settings(&conn, 2, SettingsPatch { api_key: Some("key-two".into()), ..Default::default() }).unwrap();
        let stored: String = conn.query_row("SELECT api_key FROM user_settings WHERE user_id=1", [], |r| r.get(0)).unwrap();
        assert!(stored.starts_with("v2:"));
        // 会員 1 の暗号文を会員 2 の行や別の項目に写しても、読めない（使い回せない）。
        conn.execute("UPDATE user_settings SET api_key=?1 WHERE user_id=2", [&stored]).unwrap();
        assert!(get_user_settings(&conn, 2).is_err());
        conn.execute("UPDATE user_settings SET api_key='', openai_api_key=?1 WHERE user_id=2", [&stored]).unwrap();
        assert!(get_user_settings(&conn, 2).is_err());
        // 版 6 より前の v1 の値は、移行で v2 に暗号化し直す。
        conn.execute("UPDATE user_settings SET openai_api_key='', api_key=?1 WHERE user_id=2", [crate::crypto::seal("key-two")]).unwrap();
        migrate_v6(&conn).unwrap();
        let stored: String = conn.query_row("SELECT api_key FROM user_settings WHERE user_id=2", [], |r| r.get(0)).unwrap();
        assert!(stored.starts_with("v2:"));
        assert_eq!(get_user_settings(&conn, 2).unwrap().api_key, "key-two");
        assert_eq!(get_user_settings(&conn, 1).unwrap().api_key, "key-one");
    }

    #[test]
    fn old_schema_is_migrated_to_latest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version VALUES (1);
             CREATE TABLE settings (id INTEGER PRIMARY KEY, api_key TEXT NOT NULL DEFAULT '', model TEXT NOT NULL DEFAULT '', summary_prompt TEXT NOT NULL DEFAULT '', knowledge_path TEXT NOT NULL DEFAULT '', fess_url TEXT NOT NULL DEFAULT '', fess_token TEXT NOT NULL DEFAULT '', fess_enabled INTEGER NOT NULL DEFAULT 0);
             INSERT INTO settings (id, api_key, fess_token) VALUES (1, 'legacy-plain-key', 'legacy-plain-token');
             CREATE TABLE meetings (id INTEGER PRIMARY KEY, title TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT, summary_md TEXT NOT NULL DEFAULT '', duration_sec REAL NOT NULL DEFAULT 0, finish_requested INTEGER NOT NULL DEFAULT 0, active_question_id INTEGER, error TEXT);
             CREATE TABLE questions (id INTEGER PRIMARY KEY, meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE, position INTEGER NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'open', answer TEXT NOT NULL DEFAULT '');
             CREATE TABLE segments (id INTEGER PRIMARY KEY, meeting_id INTEGER NOT NULL, start_sec REAL NOT NULL, text TEXT NOT NULL);
             CREATE TABLE audio_jobs (id INTEGER PRIMARY KEY, meeting_id INTEGER NOT NULL, start_ms INTEGER NOT NULL, wav_path TEXT NOT NULL, status TEXT NOT NULL, error TEXT, created_at TEXT NOT NULL, UNIQUE(meeting_id, start_ms));
             CREATE TABLE knowledge_hits (id INTEGER PRIMARY KEY, meeting_id INTEGER NOT NULL, source TEXT NOT NULL, title TEXT NOT NULL, locator TEXT NOT NULL, snippet TEXT NOT NULL, query TEXT NOT NULL, pinned INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL);
             CREATE TABLE templates (id INTEGER PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE template_questions (id INTEGER PRIMARY KEY, template_id INTEGER NOT NULL REFERENCES templates(id) ON DELETE CASCADE, position INTEGER NOT NULL, body TEXT NOT NULL);
             CREATE TABLE knowledge_docs (path TEXT PRIMARY KEY, title TEXT NOT NULL, mtime INTEGER NOT NULL, error TEXT);
             CREATE TABLE knowledge_chunks (id INTEGER PRIMARY KEY, path TEXT NOT NULL, title TEXT NOT NULL, heading TEXT NOT NULL, body TEXT NOT NULL);
             INSERT INTO knowledge_docs VALUES ('/k/a.md', 'a', 0, NULL);
             INSERT INTO meetings (id, title, status, created_at) VALUES (1, '旧', 'done', 'x');
             INSERT INTO segments (meeting_id, start_sec, text) VALUES (1, 0, '残る'), (99, 0, '孤立');
             INSERT INTO audio_jobs (meeting_id, start_ms, wav_path, status, created_at) VALUES (1, 0, 'a', 'done', 'x');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, LATEST_VERSION);
        let segments = list_segments(&conn, 1).unwrap();
        assert_eq!(segments.len(), 1);
        let all: i64 = conn
            .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(all, 1, "親の無い行は持ち越さない");
        assert!(column_exists(&conn, "audio_jobs", "silent").unwrap());
        assert!(has_foreign_key(&conn, "knowledge_hits").unwrap());
        assert!(get_meeting(&conn, 1).unwrap().is_some());
        assert!(column_exists(&conn, "meetings", "user_id").unwrap());
        let unowned: i64 = conn
            .query_row("SELECT COUNT(*) FROM knowledge_docs WHERE user_id IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unowned, 1, "既存の索引は持ち主未設定のまま残る");
        // 共通設定に平文で残っていたキーは暗号化され、読むと元に戻る。
        let (stored_key, stored_token): (String, String) = conn
            .query_row("SELECT api_key, fess_token FROM settings WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert!(stored_key.starts_with("v1:") && !stored_key.contains("legacy-plain"));
        assert!(stored_token.starts_with("v1:"));
        let legacy = get_settings(&conn).unwrap();
        assert_eq!(legacy.api_key, "legacy-plain-key");
        assert_eq!(legacy.fess_token, "legacy-plain-token");
        // 会員へ移したあとは、共通設定にキーを残さない。
        conn.execute(
            "INSERT INTO users (id, login_id, password_hash, created_at) VALUES (7, 'migtest', 'x', 'x')",
            [],
        )
        .unwrap();
        let dir = std::env::temp_dir();
        assign_legacy(&conn, 7, &dir).unwrap();
        assert_eq!(get_user_settings(&conn, 7).unwrap().api_key, "legacy-plain-key");
        let left: String = conn
            .query_row("SELECT api_key || fess_token FROM settings WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(left.is_empty());
    }
}
