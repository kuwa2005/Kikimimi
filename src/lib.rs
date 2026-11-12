//! ローカル会議支援。
//!
//! 録音はブラウザの音声スレッドが途切らせず、文字起こし・回答候補・ナレッジ検索は
//! 後段の待ち行列で行う。解析が遅れてもマイクは止めない。

mod accounts;
mod anthropic;
mod audio;
mod chrome;
mod crypto;
mod db;
mod export;
mod gemini;
mod jobs;
mod knowledge;
mod mail;
mod openai;
mod routes;
mod state;
mod zip;

use std::path::PathBuf;

use anyhow::Context;
use tokio::net::TcpListener;

pub use routes::router;
pub use state::AppState;

pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KIKIMIMI_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kikimimi")
}

/// 既定の待ち受けポート。使用中なら次の番号を順に試す。
/// 再起動しても同じ番号になるよう、ランダムなポートは使わない。
pub const DEFAULT_PORT: u16 = 8765;

async fn bind_listener(bind: std::net::IpAddr) -> anyhow::Result<TcpListener> {
    if let Some(port) = std::env::var("KIKIMIMI_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
    {
        return TcpListener::bind((bind, port))
            .await
            .with_context(|| format!("ポート {port} で待ち受けられません"));
    }
    let mut last = None;
    for port in DEFAULT_PORT..DEFAULT_PORT + 10 {
        match TcpListener::bind((bind, port)).await {
            Ok(listener) => return Ok(listener),
            Err(err) => last = Some(err),
        }
    }
    Err(last.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("空きポートがありません")))
        .context(format!("ポート {DEFAULT_PORT}〜{} がすべて使用中です", DEFAULT_PORT + 9))
}

pub async fn serve() -> anyhow::Result<()> {
    let data_dir = default_data_dir();
    std::fs::create_dir_all(&data_dir).with_context(|| format!("{}", data_dir.display()))?;
    crypto::init(&data_dir)?;
    let conn = db::open(&data_dir.join("kikimimi.sqlite"))?;
    let (legacy_meetings, legacy_templates) = db::legacy_counts(&conn)?;
    if legacy_meetings + legacy_templates > 0 {
        tracing::warn!(
            "持ち主が未設定の会議が {legacy_meetings} 件、テンプレートが {legacy_templates} 件あります。会員登録のあと `kikimimi admin assign-legacy <ログインID>` で持ち主を指定してください（誰にも表示されません）"
        );
    }
    let state = AppState::new(conn, data_dir);
    jobs::spawn(state.clone());

    let bind = bind_addr();
    let listener = bind_listener(bind).await?;
    let addr = listener.local_addr()?;
    let browser_url = format!("http://127.0.0.1:{}", addr.port());
    tracing::info!("会議支援を起動しました。ブラウザで開くアドレス: {browser_url}");
    if std::env::var("KIKIMIMI_NO_BROWSER").ok().as_deref() != Some("1") {
        chrome::open(&browser_url);
    } else {
        tracing::info!("ブラウザは自動で開きません。{browser_url} を Chrome で開いてください");
    }

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn bind_addr() -> std::net::IpAddr {
    match std::env::var("KIKIMIMI_BIND") {
        Ok(value) => value.parse().unwrap_or_else(|_| {
            tracing::warn!("KIKIMIMI_BIND={value} は不正です。127.0.0.1 で待ち受けます");
            std::net::Ipv4Addr::LOCALHOST.into()
        }),
        Err(_) => std::net::Ipv4Addr::LOCALHOST.into(),
    }
}

/// 運用者のコマンド。`kikimimi admin <操作> ...`
///
/// - `list-users`: 会員の一覧
/// - `assign-legacy <ログインID>`: 会員制より前の会議・テンプレート・索引・共通設定を、その会員に移す
/// - `reset-password <ログインID>`: 仮のパスワードを発行し、その会員のセッションをすべて失効させる
/// - `backup <保存先フォルダ>`: 動かしたまま、DB（一貫した写し）・secret.key・録音・資料を保存先へ写す
pub fn admin(args: &[String]) -> anyhow::Result<()> {
    let data_dir = default_data_dir();
    std::fs::create_dir_all(&data_dir)?;
    crypto::init(&data_dir)?;
    let conn = db::open(&data_dir.join("kikimimi.sqlite"))?;
    let find = |login: &str| -> anyhow::Result<i64> {
        let login = login.trim().to_ascii_lowercase();
        accounts::user_id_by_login(&conn, &login)?
            .with_context(|| format!("会員 {login} が見つかりません（先に画面から登録してください）"))
    };
    match args.first().map(String::as_str) {
        Some("list-users") => {
            for (id, login, created) in accounts::list_users(&conn)? {
                println!("{id}\t{login}\t{created}");
            }
        }
        Some("assign-legacy") => {
            let login = args.get(1).context("ログインIDを指定してください")?;
            let user_id = find(login)?;
            let backup = data_dir.join("backup").join(format!(
                "before-assign-legacy-{}.sqlite",
                chrono::Utc::now().format("%Y%m%d-%H%M%S")
            ));
            std::fs::create_dir_all(backup.parent().expect("backup dir"))?;
            conn.execute("VACUUM INTO ?1", [backup.display().to_string()])?;
            let (meetings, templates, docs) = db::assign_legacy(&conn, user_id, &data_dir)?;
            println!(
                "{login} に移しました: 会議 {meetings} 件、テンプレート {templates} 件、資料 {docs} 件（移す前の DB: {}）",
                backup.display()
            );
        }
        Some("reset-password") => {
            let login = args.get(1).context("ログインIDを指定してください")?;
            let user_id = find(login)?;
            let password = format!("tmp-{}", &crypto::random_token()[..16]);
            accounts::set_password(&conn, user_id, &accounts::hash_password(&password)?)?;
            accounts::revoke_other_sessions(&conn, user_id, None)?;
            println!("{login} の仮パスワード: {password}");
            println!("ログイン後、アカウント画面でパスワードを変えるよう本人に伝えてください。");
        }
        Some("backup") => {
            let target = std::path::PathBuf::from(args.get(1).context("保存先のフォルダを指定してください")?);
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
            let dest = target.join(format!("kikimimi-{stamp}"));
            std::fs::create_dir_all(&dest)?;
            // DB は動かしたままでも一貫した写しを作る。
            conn.execute("VACUUM INTO ?1", [dest.join("kikimimi.sqlite").display().to_string()])?;
            // secret.key が無いと、保存した API キーを読めない。必ず一緒に残す。
            std::fs::copy(data_dir.join("secret.key"), dest.join("secret.key"))?;
            let mut files = 0u64;
            for entry in std::fs::read_dir(&data_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if !entry.file_type()?.is_dir() || name == "backup" || entry.path() == target || dest.starts_with(entry.path()) {
                    continue;
                }
                files += copy_tree(&entry.path(), &dest.join(&name))?;
            }
            println!("バックアップしました: {}（DB・secret.key・ファイル {files} 件）", dest.display());
            println!("secret.key は API キーの復号に要ります。DB と同じ場所で、外に漏れないよう保管してください。");
        }
        _ => anyhow::bail!(
            "使い方: kikimimi admin list-users | assign-legacy <ログインID> | reset-password <ログインID> | backup <保存先フォルダ>"
        ),
    }
    Ok(())
}

/// フォルダを丸ごと写す。写したファイルの数を返す。
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<u64> {
    std::fs::create_dir_all(to)?;
    let mut count = 0;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            count += copy_tree(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), &target)?;
            count += 1;
        }
    }
    Ok(count)
}
