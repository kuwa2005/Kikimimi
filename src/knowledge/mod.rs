//! 社内ナレッジの検索。
//! 本文はローカル索引か、利用者が有効にした FESS にだけ問い合わせる。
//! Gemini に送るのは会議から作った短い検索語まで。

mod fess;
mod local;
mod office;

use serde::Serialize;

use crate::state::AppState;

pub use fess::{check_fess, fess_allowed};
use fess::search_fess;
use local::search_local;
pub use local::{chunk_text, commit_index, make_snippet, prepare_many, read_text};

/// 会議に添付した顧客資料の置き場所（会議ごと）。
pub fn meeting_docs_dir(state: &AppState, meeting_id: i64) -> std::path::PathBuf {
    state.data_dir().join("meeting_docs").join(meeting_id.to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct HitDraft {
    pub source: String,
    pub title: String,
    pub locator: String,
    pub snippet: String,
    pub query: String,
}

/// ローカル索引は読み取り専用の接続で引く。録音の受付が使う接続のロックを長く持たない。
async fn local_hits(state: &AppState, user_id: i64, queries: &[String]) -> Vec<HitDraft> {
    let queries = queries.to_vec();
    if let Some(reader) = state.reader() {
        return tokio::task::spawn_blocking(move || search_local(&reader, user_id, &queries))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
    }
    let conn = state.db().lock().await;
    search_local(&conn, user_id, &queries).unwrap_or_default()
}

/// 本人の索引と、本人が設定した FESS だけを検索する。
pub async fn search_all(state: &AppState, user_id: i64, queries: &[String]) -> (Vec<HitDraft>, Option<String>) {
    if queries.is_empty() {
        return (Vec::new(), None);
    }
    let local_hits = local_hits(state, user_id, queries).await;
    let settings = {
        let conn = state.db().lock().await;
        crate::db::get_user_settings(&conn, user_id).unwrap_or_default()
    };
    if !settings.fess_enabled {
        return (local_hits, None);
    }
    if settings.fess_url.is_empty() {
        return (local_hits, Some("FESS の URL が空です".into()));
    }
    match search_fess(
        state.http(),
        &settings.fess_url,
        &settings.fess_token,
        queries,
    )
    .await
    {
        Ok(remote) => {
            let mut merged = local_hits;
            merged.extend(remote);
            merged.truncate(12);
            (merged, None)
        }
        Err(err) => (local_hits, Some(format!("FESSに接続できません: {err}"))),
    }
}

/// 会議の中の検索。その会議の顧客資料（#97）を先に、本人の索引と FESS をあとに並べる。
pub async fn search_for_meeting(
    state: &AppState,
    user_id: i64,
    meeting_id: i64,
    queries: &[String],
) -> (Vec<HitDraft>, Option<String>) {
    if queries.is_empty() {
        return (Vec::new(), None);
    }
    let customer = {
        let conn = state.db().lock().await;
        crate::db::search_meeting_docs(&conn, meeting_id, queries).unwrap_or_default()
    };
    let (rest, warning) = search_all(state, user_id, queries).await;
    let mut merged = customer;
    merged.extend(rest);
    merged.truncate(15);
    (merged, warning)
}

#[derive(Debug, Clone, Serialize)]
pub struct ReindexResult {
    pub docs: i64,
    pub chunks: i64,
    pub errors: i64,
}

/// 本人がアップロードした資料と、許可されたナレッジフォルダを読み直して、本人の索引を作り直す。
pub async fn reindex(state: &AppState, user_id: i64) -> anyhow::Result<ReindexResult> {
    let path = {
        let conn = state.db().lock().await;
        crate::db::get_user_settings(&conn, user_id)?.knowledge_path
    };
    let mut roots = Vec::new();
    let uploads = uploads_dir(state, user_id);
    if uploads.is_dir() {
        roots.push(uploads);
    }
    if !path.is_empty() {
        knowledge_path_allowed(&path).map_err(|err| anyhow::anyhow!(err))?;
        roots.push(std::path::PathBuf::from(&path));
    }
    if roots.is_empty() {
        anyhow::bail!("索引する資料がありません。資料をアップロードするか、ナレッジフォルダを設定してください");
    }
    let prepared = tokio::task::spawn_blocking(move || prepare_many(&roots)).await??;
    let conn = state.db().lock().await;
    let (docs, chunks, errors) = commit_index(&conn, user_id, &prepared)?;
    Ok(ReindexResult {
        docs,
        chunks,
        errors,
    })
}

/// 運用者が許可したフォルダ（`KIKIMIMI_KNOWLEDGE_ROOT`、カンマ区切り）の配下か。
/// 未設定なら、サーバ上のフォルダは指定させない（会員ごとのアップロードだけ使える）。
pub fn knowledge_path_allowed(path: &str) -> Result<(), String> {
    let roots: Vec<String> = std::env::var("KIKIMIMI_KNOWLEDGE_ROOT")
        .unwrap_or_default()
        .split(',')
        .map(|root| root.trim().to_string())
        .filter(|root| !root.is_empty())
        .collect();
    if path.trim().is_empty() {
        return Ok(());
    }
    if roots.is_empty() {
        return Err("サーバ上のフォルダは指定できません。資料はアップロードしてください".into());
    }
    let target = std::path::Path::new(path.trim())
        .canonicalize()
        .map_err(|_| format!("フォルダがありません: {}", path.trim()))?;
    let inside = roots.iter().any(|root| {
        std::path::Path::new(root)
            .canonicalize()
            .is_ok_and(|root| target.starts_with(root))
    });
    if inside {
        Ok(())
    } else {
        Err(format!("ナレッジフォルダは {} の配下だけ指定できます", roots.join(", ")))
    }
}

// ---- 本人の資料（アップロード） ----

pub const UPLOAD_MAX_BYTES: usize = 8 * 1024 * 1024;
const UPLOAD_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Upload {
    pub name: String,
    pub bytes: u64,
    pub uploaded_at: String,
}

pub fn uploads_dir(state: &AppState, user_id: i64) -> std::path::PathBuf {
    state.data_dir().join("knowledge").join(user_id.to_string())
}

/// ファイル名はそのフォルダの直下の名前だけを受け付ける（`..` や区切り文字は不可）。
pub fn clean_upload_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bad = name.is_empty()
        || name.starts_with('.')
        || name.chars().count() > 120
        || name.chars().any(|ch| ch.is_control() || "/\\:*?\"<>|".contains(ch));
    if bad {
        return Err("ファイル名が使えません".into());
    }
    if matches!(ext.as_str(), "doc" | "xls") {
        return Err("旧形式（.doc / .xls）は読めません。docx / xlsx か PDF で保存し直してください".into());
    }
    if !matches!(ext.as_str(), "md" | "txt" | "pdf" | "docx" | "xlsx") {
        return Err("アップロードできるのは .md / .txt / .pdf / .docx / .xlsx だけです".into());
    }
    Ok(name.to_string())
}

pub fn list_uploads(dir: &std::path::Path) -> Vec<Upload> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Upload> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let uploaded_at = meta
                .modified()
                .ok()
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default();
            Some(Upload {
                name,
                bytes: meta.len(),
                uploaded_at,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub async fn save_upload(dir: &std::path::Path, name: &str, body: &[u8]) -> Result<(), String> {
    let name = clean_upload_name(name)?;
    if body.len() > UPLOAD_MAX_BYTES {
        return Err("1 ファイル 8MB までです".into());
    }
    let existing: u64 = list_uploads(dir)
        .iter()
        .filter(|upload| upload.name != name)
        .map(|upload| upload.bytes)
        .sum();
    if existing + body.len() as u64 > UPLOAD_TOTAL_BYTES {
        return Err("資料は合計 100MB までです".into());
    }
    tokio::fs::create_dir_all(dir).await.map_err(|err| err.to_string())?;
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&tmp, body).await.map_err(|err| err.to_string())?;
    tokio::fs::rename(&tmp, dir.join(&name))
        .await
        .map_err(|err| err.to_string())
}

pub async fn delete_upload(dir: &std::path::Path, name: &str) -> Result<(), String> {
    let name = clean_upload_name(name)?;
    tokio::fs::remove_file(dir.join(name))
        .await
        .map_err(|_| "資料が見つかりません".to_string())
}
