use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::HitDraft;

const CHUNK_CHARS: usize = 800;
const OVERLAP_CHARS: usize = 150;
const MAX_FILES: usize = 5000;
const MAX_BYTES: u64 = 8_000_000;

pub struct Prepared {
    pub docs: Vec<IndexedDoc>,
    pub chunks: Vec<IndexedChunk>,
}

pub struct IndexedDoc {
    pub path: String,
    pub title: String,
    pub mtime: i64,
    pub error: Option<String>,
}

pub struct IndexedChunk {
    pub path: String,
    pub title: String,
    pub heading: String,
    pub body: String,
}

#[cfg(test)]
pub fn prepare(root: &str) -> Result<Prepared> {
    prepare_many(&[PathBuf::from(root)])
}

/// 複数のフォルダ（本人のアップロード領域と、許可されたナレッジフォルダ）をまとめて読む。
pub fn prepare_many(roots: &[PathBuf]) -> Result<Prepared> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for root in roots {
        if !root.is_dir() {
            anyhow::bail!("フォルダがありません: {}", root.display());
        }
        let root = root.canonicalize().context("フォルダを解決できません")?;
        collect(&root, &root, 0, &mut files, &mut skipped);
    }
    if files.len() > MAX_FILES {
        anyhow::bail!("ファイルが{MAX_FILES}件を超えました。対象フォルダを絞ってください");
    }
    let mut docs = skipped;
    let mut chunks = Vec::new();
    for path in files {
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                docs.push(error_doc(&path, err.to_string()));
                continue;
            }
        };
        if meta.len() > MAX_BYTES {
            docs.push(error_doc(&path, "ファイルが大きすぎます".into()));
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_secs() as i64)
            .unwrap_or(0);
        let text = match read_text(&path) {
            Ok(text) => text,
            Err(err) => {
                docs.push(IndexedDoc {
                    path: path.display().to_string(),
                    title: file_title(&path),
                    mtime,
                    error: Some(err),
                });
                continue;
            }
        };
        let title = doc_title(&path, &text);
        docs.push(IndexedDoc {
            path: path.display().to_string(),
            title: title.clone(),
            mtime,
            error: None,
        });
        for (heading, body) in chunk_text(&text) {
            let body = body.trim().to_string();
            if body.chars().count() < 2 {
                continue;
            }
            chunks.push(IndexedChunk {
                path: path.display().to_string(),
                title: title.clone(),
                heading,
                body,
            });
        }
    }
    Ok(Prepared { docs, chunks })
}

/// その会員の索引だけを作り直す。他の会員の索引には触れない。
pub fn commit_index(conn: &Connection, user_id: i64, prepared: &Prepared) -> Result<(i64, i64, i64)> {
    let tx = conn.unchecked_transaction()?;
    crate::db::clear_index(&tx, user_id)?;
    for doc in &prepared.docs {
        tx.execute(
            "INSERT INTO knowledge_docs (user_id, path, title, mtime, error) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id, path) DO NOTHING",
            params![user_id, doc.path, doc.title, doc.mtime, doc.error],
        )?;
    }
    for chunk in &prepared.chunks {
        tx.execute(
            "INSERT INTO knowledge_chunks (user_id, path, title, heading, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user_id, chunk.path, chunk.title, chunk.heading, chunk.body],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO knowledge_fts (rowid, heading, body) VALUES (?1, ?2, ?3)",
            params![id, chunk.heading, chunk.body],
        )?;
    }
    tx.commit()?;
    let docs = prepared.docs.len() as i64;
    let chunks = prepared.chunks.len() as i64;
    let errors = prepared
        .docs
        .iter()
        .filter(|doc| doc.error.is_some())
        .count() as i64;
    Ok((docs, chunks, errors))
}

/// 本人の索引だけを引く（SQL の条件で絞る。全員の結果を取ってから画面で隠すことはしない）。
pub fn search_local(conn: &Connection, user_id: i64, queries: &[String]) -> Result<Vec<HitDraft>> {
    let mut hits = Vec::new();
    for query in queries {
        let mut found_fts = false;
        if query.chars().count() >= 3 {
            match search_fts(conn, user_id, query) {
                Ok(found) => {
                    found_fts = !found.is_empty();
                    push_unique(&mut hits, found);
                }
                Err(err) => tracing::warn!("全文検索に失敗したため、部分一致で探します: {err:#}"),
            }
        }
        // 全件をなめる部分一致は、全文索引で見つからないときと 2 文字の語だけ。
        if !found_fts && query.chars().count() >= 2 {
            push_unique(&mut hits, search_like(conn, user_id, query)?);
        }
        if hits.len() >= 8 {
            break;
        }
    }
    hits.truncate(8);
    Ok(hits)
}

pub fn search_fts(conn: &Connection, user_id: i64, query: &str) -> Result<Vec<HitDraft>> {
    let mut stmt = conn.prepare(
        "SELECT c.path, c.title, c.heading, c.body
         FROM knowledge_fts
         JOIN knowledge_chunks AS c ON c.id = knowledge_fts.rowid
         WHERE knowledge_fts MATCH ?1 AND c.user_id = ?2
         ORDER BY bm25(knowledge_fts)
         LIMIT 5",
    )?;
    let escaped = query.replace('"', "\"\"");
    let phrase = format!("\"{escaped}\"");
    let rows = stmt.query_map(params![phrase, user_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (path, title, heading, body) = row?;
        hits.push(hit_from(path, title, heading, body, query));
    }
    Ok(hits)
}

fn search_like(conn: &Connection, user_id: i64, query: &str) -> Result<Vec<HitDraft>> {
    let mut stmt = conn.prepare(
        "SELECT path, title, heading, body FROM knowledge_chunks
         WHERE user_id = ?2 AND (body LIKE ?1 ESCAPE '\\' OR heading LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\')
         LIMIT 5",
    )?;
    let pattern = like_contains(query);
    let rows = stmt.query_map(params![pattern, user_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (path, title, heading, body) = row?;
        hits.push(hit_from(path, title, heading, body, query));
    }
    Ok(hits)
}

fn hit_from(path: String, title: String, heading: String, body: String, query: &str) -> HitDraft {
    let mut snippet = make_snippet(&body, query);
    if !heading.is_empty() {
        snippet = format!("{heading} — {snippet}");
    }
    HitDraft {
        source: "local".into(),
        title,
        locator: path,
        snippet,
        query: query.to_string(),
    }
}

pub fn make_snippet(body: &str, query: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let needle: Vec<char> = query.chars().collect();
    let pos = if needle.is_empty() {
        None
    } else {
        chars
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
    };
    let (start, end) = if let Some(pos) = pos {
        let start = pos.saturating_sub(40);
        let end = (pos + needle.len() + 80).min(chars.len());
        (start, end)
    } else {
        (0, chars.len().min(120))
    };
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

pub fn chunk_text(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut heading = String::new();
    let mut body = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            if !body.trim().is_empty() || !heading.is_empty() {
                sections.push((heading.clone(), body.clone()));
            }
            heading = trimmed.trim_start_matches('#').trim().to_string();
            body.clear();
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if !body.trim().is_empty() || !heading.is_empty() {
        sections.push((heading, body));
    }
    if sections.is_empty() {
        sections.push((String::new(), text.to_string()));
    }
    let mut chunks = Vec::new();
    for (heading, body) in sections {
        for piece in windows(&body, CHUNK_CHARS, OVERLAP_CHARS) {
            chunks.push((heading.clone(), piece));
        }
    }
    chunks
}

fn windows(text: &str, size: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= size {
        return vec![chars.into_iter().collect()];
    }
    let step = size.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let end = (index + size).min(chars.len());
        out.push(chars[index..end].iter().collect());
        if end == chars.len() {
            break;
        }
        index += step;
    }
    out
}

/// 読めないフォルダやファイルは `skipped` に記録して続ける。1 か所の権限エラーで全体を止めない。
fn collect(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
    skipped: &mut Vec<IndexedDoc>,
) {
    if depth > 20 || out.len() > MAX_FILES {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            skipped.push(error_doc(dir, format!("フォルダを読めません: {err}")));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                skipped.push(error_doc(dir, format!("フォルダを読めません: {err}")));
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                skipped.push(error_doc(&path, err.to_string()));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect(root, &path, depth + 1, out, skipped);
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "md" | "txt" | "pdf" | "docx" | "xlsx") {
            let canonical = path.canonicalize().unwrap_or(path);
            if canonical.starts_with(root) {
                out.push(canonical);
            }
        }
    }
}

pub fn read_text(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "pdf" {
        // pdf-extract は壊れた PDF で panic することがある。1 ファイルの失敗にとどめる。
        let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text(path));
        match extracted {
            Ok(Ok(text)) if !text.trim().is_empty() => Ok(text),
            Ok(Ok(_)) => Err("テキストを抽出できません".into()),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err("PDF を読み取れません（壊れている可能性があります）".into()),
        }
    } else if ext == "docx" {
        super::office::docx_text(&fs::read(path).map_err(|err| err.to_string())?)
    } else if ext == "xlsx" {
        super::office::xlsx_text(&fs::read(path).map_err(|err| err.to_string())?)
    } else if ext == "doc" || ext == "xls" {
        Err("旧形式（.doc / .xls）は読めません。docx / xlsx か PDF で保存し直してください".into())
    } else {
        fs::read_to_string(path).map_err(|err| err.to_string())
    }
}

fn error_doc(path: &Path, error: String) -> IndexedDoc {
    IndexedDoc {
        path: path.display().to_string(),
        title: file_title(path),
        mtime: 0,
        error: Some(error),
    }
}

fn file_title(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("無題")
        .to_string()
}

fn doc_title(path: &Path, text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim_start_matches('#').trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    file_title(path)
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

fn push_unique(dest: &mut Vec<HitDraft>, found: Vec<HitDraft>) {
    for hit in found {
        if dest.len() >= 8 {
            return;
        }
        let exists = dest
            .iter()
            .any(|item| item.locator == hit.locator && item.snippet == hit.snippet);
        if !exists {
            dest.push(hit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_overlaps() {
        let text = "あ".repeat(1000);
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].1.chars().count() <= 800);
    }

    #[test]
    fn heading_becomes_section() {
        let chunks = chunk_text("# 予算\n\n来期は増額する。\n");
        assert_eq!(chunks[0].0, "予算");
        assert!(chunks[0].1.contains("来期は増額"));
    }

    #[test]
    fn fts_and_short_query() {
        let dir = std::env::temp_dir().join(format!("kikimimi-idx-{}", uuid_name()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("budget.md"),
            "# 来期予算\n\n来期予算は増額する方針です。\n",
        )
        .unwrap();
        let prepared = prepare(dir.to_str().unwrap()).unwrap();
        let db_path = dir.join("t.sqlite");
        let conn = crate::db::open(&db_path).unwrap();
        let hash = crate::accounts::hash_password("password-for-test").unwrap();
        let user = crate::accounts::create_user(&conn, "alice", &hash).unwrap().unwrap();
        let other = crate::accounts::create_user(&conn, "bob", &hash).unwrap().unwrap();
        let (docs, chunks, errors) = commit_index(&conn, user, &prepared).unwrap();
        assert_eq!(errors, 0);
        assert!(docs >= 1);
        assert!(chunks >= 1);
        let fts = search_fts(&conn, user, "来期予算").unwrap();
        assert!(!fts.is_empty(), "全文索引そのものが引けること");
        let hits = search_local(&conn, user, &["来期予算".into()]).unwrap();
        assert!(!hits.is_empty());
        let short = search_local(&conn, user, &["予算".into()]).unwrap();
        assert!(!short.is_empty(), "2文字の語も本文検索で拾う");
        assert!(search_local(&conn, other, &["来期予算".into(), "予算".into()]).unwrap().is_empty(), "他の会員の索引は引けない");
        commit_index(&conn, other, &prepared).unwrap();
        crate::db::clear_index(&conn, other).unwrap();
        assert!(!search_local(&conn, user, &["来期予算".into()]).unwrap().is_empty(), "他の会員の再索引で消えない");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_pdf_and_unreadable_entries_do_not_stop_indexing() {
        let dir = std::env::temp_dir().join(format!("kikimimi-bad-{}", uuid_name()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("broken.pdf"), b"%PDF-1.4 not really a pdf").unwrap();
        fs::write(dir.join("ok.md"), "# 手順\n\n発注の手順です。\n").unwrap();
        let prepared = prepare(dir.to_str().unwrap()).unwrap();
        assert!(prepared.docs.iter().any(|doc| doc.error.is_some()));
        assert!(prepared.chunks.iter().any(|chunk| chunk.body.contains("発注")));
        let _ = fs::remove_dir_all(&dir);
    }

    fn uuid_name() -> String {
        format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}
