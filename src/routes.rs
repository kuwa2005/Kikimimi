use std::net::SocketAddr;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;

use crate::accounts::{self, LimitExceeded};
use crate::audio;
use crate::db::{self, SettingsPatch};
use crate::export;
use crate::jobs;
use crate::knowledge;
use crate::state::AppState;

const SESSION_COOKIE: &str = "kikimimi_session";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/auth/register/code", post(register_code))
        .route("/api/auth/register/verify", post(register_verify))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/password/forgot", post(forgot_password))
        .route("/api/auth/password/reset", post(reset_password))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/password", post(change_password))
        .route("/api/account", get(account))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/settings/check-api-key", post(check_api_key))
        .route("/api/settings/check-fess", post(check_fess))
        .route("/api/templates", get(list_templates).post(create_template))
        .route(
            "/api/templates/:id",
            axum::routing::put(update_template).delete(delete_template),
        )
        .route("/api/meetings", get(list_meetings).post(create_meeting))
        .route("/api/meetings/order", axum::routing::put(reorder_meetings))
        .route("/api/folders", get(list_folders).post(create_folder))
        .route(
            "/api/folders/:folder",
            axum::routing::patch(rename_folder).delete(delete_folder),
        )
        .route("/api/folders/:folder/export", get(export_folder))
        .route("/api/meetings/:id/folder", post(move_meeting))
        .route("/api/meetings/:id/marks", post(add_mark))
        .route("/api/meetings/:id/marks/:mark_id", axum::routing::delete(delete_mark))
        .route("/api/meetings/:id/notes/:note_id", axum::routing::delete(delete_note))
        .route("/api/meetings/:id/recap", post(recap))
        .route("/api/meetings/:id/import-text", post(import_text))
        .route("/api/meetings/:id/docs", get(list_meeting_docs).post(upload_meeting_doc))
        .route(
            "/api/meetings/:id/docs/:doc_id",
            get(download_meeting_doc).delete(delete_meeting_doc),
        )
        .route(
            "/api/meetings/:id",
            get(get_meeting)
                .put(update_meeting)
                .patch(patch_meeting)
                .delete(remove_meeting),
        )
        .route("/api/meetings/:id/start", post(start_meeting))
        .route("/api/meetings/:id/pause", post(pause_meeting))
        .route("/api/meetings/:id/resume", post(resume_meeting))
        .route("/api/meetings/:id/finish", post(finish_meeting))
        .route("/api/meetings/:id/speech", post(set_speech))
        .route("/api/meetings/:id/captions", post(save_caption))
        .route("/api/meetings/:id/cancel-finish", post(cancel_finish))
        .route("/api/meetings/:id/retry-audio", post(retry_audio))
        .route("/api/meetings/:id/give-up-retries", post(give_up_retries))
        .route("/api/meetings/:id/resummarize", post(resummarize))
        .route("/api/meetings/:id/summary", axum::routing::put(put_summary))
        .route("/api/meetings/:id/questions", post(add_question))
        .route(
            "/api/meetings/:id/questions/:qid",
            axum::routing::patch(edit_question).delete(delete_question),
        )
        .route(
            "/api/meetings/:id/segments/:sid",
            axum::routing::patch(edit_segment),
        )
        .route("/api/meetings/:id/audio", post(post_audio).get(get_audio))
        .route("/api/meetings/:id/events", get(events))
        .route("/api/meetings/:id/export", get(export_markdown))
        .route("/api/meetings/:id/knowledge", get(search_meeting_knowledge))
        .route("/api/meetings/:id/hits/:hit_id/pin", post(pin_hit))
        .route("/api/knowledge/files", get(list_uploads).post(upload_file))
        .route(
            "/api/knowledge/files/:name",
            axum::routing::delete(delete_upload),
        )
        .route("/api/knowledge/reindex", post(reindex))
        .route("/api/knowledge/search", get(search_knowledge))
        .fallback(static_file)
        .layer(DefaultBodyLimit::max(9 * 1024 * 1024))
        .layer(from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

// ---- 認証 ----

/// セッションから決まった、いま操作している会員。ブラウザが送る user_id では決めない。
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub login_id: String,
    pub csrf: String,
    pub session_id: i64,
}

/// 長さに依存しない比較。
fn same_secret(left: &str, right: &str) -> bool {
    let (a, b) = (left.as_bytes(), right.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
}

fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "ログインしてください。", "code": "unauthenticated"})),
    )
        .into_response()
}

async fn auth(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if !path.starts_with("/api/")
        || path == "/api/auth/register"
        || path == "/api/auth/register/code"
        || path == "/api/auth/register/verify"
        || path == "/api/auth/login"
        || path == "/api/auth/password/forgot"
        || path == "/api/auth/password/reset"
    {
        return next.run(request).await;
    }
    let Some(token) = cookie_value(request.headers(), SESSION_COOKIE) else {
        return unauthenticated();
    };
    let session = {
        let conn = state.db().lock().await;
        accounts::session(&conn, &token).ok().flatten()
    };
    let Some(session) = session else {
        return unauthenticated();
    };
    // Cookie で認証するので、状態を変える要求には CSRF トークンを求める。
    if request.method() != Method::GET && request.method() != Method::HEAD {
        let presented = request
            .headers()
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !same_secret(presented, &session.csrf) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "画面を再読み込みしてから操作してください。", "code": "csrf"})),
            )
                .into_response();
        }
    }
    request.extensions_mut().insert(CurrentUser {
        id: session.user_id,
        login_id: session.login_id,
        csrf: session.csrf,
        session_id: session.id,
    });
    next.run(request).await
}

fn secure_cookie(headers: &HeaderMap) -> bool {
    std::env::var("KIKIMIMI_SECURE_COOKIE").ok().as_deref() == Some("1")
        || headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

fn session_cookie(token: &str, headers: &HeaderMap) -> HeaderValue {
    let secure = if secure_cookie(headers) { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
        accounts::SESSION_DAYS * 24 * 3600
    ))
    .expect("cookie")
}

fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static("kikimimi_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

fn client_ip(connect: Option<ConnectInfo<SocketAddr>>, headers: &HeaderMap) -> String {
    if std::env::var("KIKIMIMI_TRUST_PROXY").ok().as_deref() == Some("1") {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
        {
            return forwarded.trim().to_string();
        }
    }
    connect
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "local".into())
}

fn rate_limited(wait: u64) -> AppError {
    AppError::new(
        StatusCode::TOO_MANY_REQUESTS,
        format!("試行が多すぎます。{wait} 秒ほど待ってからやり直してください。"),
    )
    .code("rate_limited")
    .extra(json!({"retry_after": wait}))
}

#[derive(Deserialize)]
struct Credentials {
    login_id: String,
    password: String,
}

#[derive(Deserialize)]
struct RegisterCodeBody {
    login_id: String,
    #[serde(default)]
    invite: String,
}

#[derive(Deserialize)]
struct EmailBody {
    login_id: String,
}

#[derive(Deserialize)]
struct CodeCheckBody {
    login_id: String,
    code: String,
}

#[derive(Deserialize)]
struct ResetBody {
    login_id: String,
    code: String,
    password: String,
}

#[derive(Deserialize)]
struct RegisterBody {
    login_id: String,
    password: String,
    code: String,
    #[serde(default)]
    invite: String,
}

fn email_of(raw: &str) -> Result<String, AppError> {
    accounts::normalize_email(raw).map_err(|code| {
        bad("ログインID（メールアドレス）の形になっていません。例: ada@example.com").code(code)
    })
}

fn password_rule(password: &str) -> Result<(), AppError> {
    accounts::check_password(password).map_err(|code| {
        bad("パスワードは 8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）").code(code)
    })
}

fn require_invite(invite: &str) -> Result<(), AppError> {
    if accounts::invite_matches(invite) {
        Ok(())
    } else {
        Err(bad("招待コードが違います。").code("invalid_invite"))
    }
}

fn six_digits(raw: &str) -> Result<String, AppError> {
    let code = raw.trim().to_string();
    if code.len() == 6 && code.chars().all(|ch| ch.is_ascii_digit()) {
        Ok(code)
    } else {
        Err(signup_code_error("invalid_code"))
    }
}

fn mail_problem(err: crate::mail::MailError) -> AppError {
    match err {
        crate::mail::MailError::Unconfigured => {
            bad("確認メールを送る設定がありません。運用者に連絡してください。").code("mail_unconfigured")
        }
        crate::mail::MailError::Failed(message) => {
            tracing::warn!(error = %message, "確認メールを送れませんでした");
            bad("確認メールを送れませんでした。しばらくしてからやり直してください。").code("mail_failed")
        }
    }
}

fn signup_code_error(code: &str) -> AppError {
    match code {
        "code_expired" => bad("確認コードの期限が切れました。もう一度メールを送ってください。").code("code_expired"),
        "code_locked" => bad("確認コードを何度も間違えました。もう一度メールを送ってください。").code("code_locked"),
        "password_changed" => {
            bad("パスワードが、確認コードを送ったときと違います。最初からやり直してください。").code("password_changed")
        }
        _ => bad("確認コードが違います。メールに届いた6桁の数字を入力してください。").code("invalid_code"),
    }
}

async fn register_code(
    State(state): State<AppState>,
    connect: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<RegisterCodeBody>,
) -> Result<Json<Value>, AppError> {
    let ip = client_ip(connect, &headers);
    state
        .throttle(&format!("register-code:{ip}"), 10, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    state
        .throttle("register-code:all", 200, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    let login_id = email_of(&body.login_id)?;
    state
        .throttle(&format!("register-code:{login_id}"), 5, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    require_invite(&body.invite)?;
    let code = {
        let conn = state.db().lock().await;
        if accounts::user_id_by_login(&conn, &login_id).map_err(internal)?.is_some() {
            return Err(
                AppError::new(StatusCode::CONFLICT, "このメールアドレスはすでに登録されています").code("login_taken"),
            );
        }
        let code = accounts::new_signup_code();
        accounts::store_signup_code(&conn, &login_id, &code, "").map_err(internal)?;
        code
    };
    if let Err(err) = tokio::task::spawn_blocking({
        let login_id = login_id.clone();
        let code = code.clone();
        move || crate::mail::send_signup_code(&login_id, &code)
    })
    .await
    .map_err(|err| internal(err.into()))?
    {
        let conn = state.db().lock().await;
        let _ = accounts::delete_signup_code(&conn, &login_id);
        return Err(mail_problem(err));
    }
    tracing::info!("登録の確認コードを送りました");
    #[cfg(test)]
    {
        return Ok(Json(json!({"sent": true, "code": code})));
    }
    #[cfg(not(test))]
    {
        let _ = code;
        Ok(Json(json!({"sent": true})))
    }
}

async fn register(
    State(state): State<AppState>,
    connect: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<RegisterBody>,
) -> Result<Response, AppError> {
    let ip = client_ip(connect, &headers);
    state
        .throttle(&format!("register:{ip}"), 30, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    let login_id = email_of(&body.login_id)?;
    password_rule(&body.password)?;
    require_invite(&body.invite)?;
    let code = six_digits(&body.code)?;
    {
        let conn = state.db().lock().await;
        accounts::match_signup_code(&conn, &login_id, &code).map_err(internal)?.map_err(signup_code_error)?;
    }
    let password = body.password.clone();
    let hash = tokio::task::spawn_blocking(move || accounts::hash_password(&password))
        .await
        .map_err(|err| internal(err.into()))?
        .map_err(internal)?;
    let created = {
        let conn = state.db().lock().await;
        let created = accounts::create_user(&conn, &login_id, &hash).map_err(internal)?;
        if created.is_some() {
            accounts::delete_signup_code(&conn, &login_id).map_err(internal)?;
        }
        created
    };
    let Some(user_id) = created else {
        return Err(
            AppError::new(StatusCode::CONFLICT, "このメールアドレスはすでに登録されています").code("login_taken"),
        );
    };
    tracing::info!(user_id, "会員を登録しました");
    signed_in(&state, user_id, &headers, StatusCode::CREATED).await
}

async fn register_verify(
    State(state): State<AppState>,
    Json(body): Json<CodeCheckBody>,
) -> Result<Json<Value>, AppError> {
    let login_id = email_of(&body.login_id)?;
    let code = six_digits(&body.code)?;
    let conn = state.db().lock().await;
    accounts::match_signup_code(&conn, &login_id, &code)
        .map_err(internal)?
        .map_err(signup_code_error)?;
    Ok(Json(json!({"ok": true})))
}

async fn forgot_password(
    State(state): State<AppState>,
    connect: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<EmailBody>,
) -> Result<Json<Value>, AppError> {
    let ip = client_ip(connect, &headers);
    state
        .throttle(&format!("forgot:{ip}"), 10, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    state
        .throttle("forgot:all", 200, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    let login_id = email_of(&body.login_id)?;
    state
        .throttle(&format!("forgot:{login_id}"), 5, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    let code = {
        let conn = state.db().lock().await;
        if accounts::user_id_by_login(&conn, &login_id).map_err(internal)?.is_none() {
            None
        } else {
            let code = accounts::new_signup_code();
            accounts::store_reset_code(&conn, &login_id, &code).map_err(internal)?;
            Some(code)
        }
    };
    let Some(code) = code else {
        return Ok(Json(json!({"sent": true})));
    };
    if let Err(err) = tokio::task::spawn_blocking({
        let login_id = login_id.clone();
        let code = code.clone();
        move || crate::mail::send_reset_code(&login_id, &code)
    })
    .await
    .map_err(|err| internal(err.into()))?
    {
        let conn = state.db().lock().await;
        let _ = accounts::delete_reset_code(&conn, &login_id);
        return Err(mail_problem(err));
    }
    tracing::info!("パスワード再設定の確認コードを送りました");
    #[cfg(test)]
    {
        return Ok(Json(json!({"sent": true, "code": code})));
    }
    #[cfg(not(test))]
    {
        let _ = code;
        Ok(Json(json!({"sent": true})))
    }
}

async fn reset_password(
    State(state): State<AppState>,
    connect: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<ResetBody>,
) -> Result<Response, AppError> {
    let ip = client_ip(connect, &headers);
    state
        .throttle(&format!("reset:{ip}"), 30, Duration::from_secs(3600))
        .map_err(rate_limited)?;
    let login_id = email_of(&body.login_id)?;
    password_rule(&body.password)?;
    let code = six_digits(&body.code)?;
    {
        let conn = state.db().lock().await;
        accounts::match_reset_code(&conn, &login_id, &code)
            .map_err(internal)?
            .map_err(signup_code_error)?;
    }
    let password = body.password.clone();
    let hash = tokio::task::spawn_blocking(move || accounts::hash_password(&password))
        .await
        .map_err(|err| internal(err.into()))?
        .map_err(internal)?;
    let user_id = {
        let conn = state.db().lock().await;
        let user_id = accounts::user_id_by_login(&conn, &login_id).map_err(internal)?;
        let Some(user_id) = user_id else {
            let _ = accounts::delete_reset_code(&conn, &login_id);
            return Err(signup_code_error("invalid_code"));
        };
        accounts::set_password(&conn, user_id, &hash).map_err(internal)?;
        accounts::revoke_other_sessions(&conn, user_id, None).map_err(internal)?;
        accounts::delete_reset_code(&conn, &login_id).map_err(internal)?;
        user_id
    };
    tracing::info!(user_id, "パスワードを再設定しました");
    signed_in(&state, user_id, &headers, StatusCode::OK).await
}

async fn signed_in(
    state: &AppState,
    user_id: i64,
    headers: &HeaderMap,
    status: StatusCode,
) -> Result<Response, AppError> {
    let (token, csrf, user) = {
        let conn = state.db().lock().await;
        let (token, csrf) = accounts::create_session(&conn, user_id).map_err(internal)?;
        let user = accounts::user(&conn, user_id).map_err(internal)?;
        (token, csrf, user)
    };
    let mut response = (status, Json(json!({"user": user, "csrf": csrf}))).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&token, headers));
    Ok(response)
}

async fn login(
    State(state): State<AppState>,
    connect: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, AppError> {
    let ip = client_ip(connect, &headers);
    let login_id = body.login_id.trim().to_ascii_lowercase();
    state
        .throttle(&format!("login-ip:{ip}"), 30, Duration::from_secs(600))
        .map_err(rate_limited)?;
    state
        .throttle(&format!("login-id:{login_id}"), 10, Duration::from_secs(600))
        .map_err(rate_limited)?;
    let found = {
        let conn = state.db().lock().await;
        accounts::credentials(&conn, &login_id).map_err(internal)?
    };
    let password = body.password.clone();
    let verified = match found {
        Some(found) if found.active => {
            let hash = found.password_hash.clone();
            tokio::task::spawn_blocking(move || accounts::verify_password(&hash, &password))
                .await
                .unwrap_or(false)
                .then_some(found.id)
        }
        _ => {
            // 存在しないIDでも同じだけ時間を掛け、IDの有無を応答時間から読ませない。
            let _ = tokio::task::spawn_blocking(move || accounts::dummy_verify(&password)).await;
            None
        }
    };
    let Some(user_id) = verified else {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "ログインIDかパスワードが違います",
        )
        .code("invalid_credentials"));
    };
    state.forget(&format!("login-id:{login_id}"));
    signed_in(&state, user_id, &headers, StatusCode::OK).await
}

async fn logout(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Response, AppError> {
    {
        let conn = state.db().lock().await;
        accounts::revoke_session(&conn, user.session_id).map_err(internal)?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(header::SET_COOKIE, clear_cookie());
    Ok(response)
}

async fn me(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    let found = accounts::user(&conn, user.id).map_err(internal)?;
    Ok(Json(json!({"user": found, "csrf": user.csrf})))
}

#[derive(Deserialize)]
struct PasswordBody {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<PasswordBody>,
) -> Result<StatusCode, AppError> {
    state
        .throttle(&format!("password:{}", user.id), 10, Duration::from_secs(600))
        .map_err(rate_limited)?;
    accounts::check_password(&body.new_password)
        .map_err(|code| bad("新しいパスワードは 8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）").code(code))?;
    let stored = {
        let conn = state.db().lock().await;
        accounts::credentials(&conn, &user.login_id)
            .map_err(internal)?
            .ok_or_else(|| not_found("会員が見つかりません"))?
            .password_hash
    };
    let current = body.current_password.clone();
    let ok = tokio::task::spawn_blocking(move || accounts::verify_password(&stored, &current))
        .await
        .unwrap_or(false);
    if !ok {
        return Err(
            AppError::new(StatusCode::UNAUTHORIZED, "今のパスワードが違います")
                .code("invalid_credentials"),
        );
    }
    let password = body.new_password.clone();
    let hash = tokio::task::spawn_blocking(move || accounts::hash_password(&password))
        .await
        .map_err(|err| internal(err.into()))?
        .map_err(internal)?;
    let conn = state.db().lock().await;
    accounts::set_password(&conn, user.id, &hash).map_err(internal)?;
    accounts::revoke_other_sessions(&conn, user.id, Some(user.session_id)).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn account(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    let found = accounts::user(&conn, user.id).map_err(internal)?;
    let plan = accounts::plan_for_user(&conn, user.id).map_err(internal)?;
    let usage = accounts::usage(&conn, user.id, &plan.limits).map_err(internal)?;
    Ok(Json(json!({
        "user": found,
        "plan": plan,
        "usage": usage,
        "note": "このサービスの Free プランと、Gemini / OpenAI の API 料金・利用枠は別です。文字起こしと要約は、あなたが登録した API キーの契約で実行され、提供元の料金が発生することがあります。"
    })))
}

// ---- 静的ファイル（web/dist） ----

fn web_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("KIKIMIMI_WEB_DIR") {
        return dir.into();
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("web")))
        .filter(|dir| dir.join("index.html").is_file())
    {
        return dir;
    }
    std::path::PathBuf::from("web/dist")
}

fn content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 画面のファイル。`/api` 以外は web/dist から返し、無ければ index.html（ハッシュでの画面遷移）。
async fn static_file(uri: axum::http::Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return not_found("API がありません").into_response();
    }
    let root = web_dir();
    let relative = path.trim_start_matches('/');
    let safe = !relative
        .split('/')
        .any(|part| part == ".." || part.starts_with('.'));
    let candidate = root.join(if relative.is_empty() || !safe {
        "index.html"
    } else {
        relative
    });
    let file = if candidate.is_file() {
        candidate
    } else {
        root.join("index.html")
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            // ハッシュ付きの名前の資産は長く、HTML などは毎回確かめる。
            let cache = if relative.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-store"
            };
            (
                [
                    (header::CONTENT_TYPE, content_type(&file)),
                    (header::CACHE_CONTROL, cache),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "画面のファイルがありません。web をビルドしてください（README の「開発」を参照）。",
        )
            .into_response(),
    }
}

async fn health() -> &'static str {
    "ok"
}

// ---- エラー ----

struct AppError {
    status: StatusCode,
    message: String,
    code: Option<&'static str>,
    extra: Option<Value>,
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            extra: None,
        }
    }

    fn code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    fn extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut body = json!({"error": self.message});
        if let Some(code) = self.code {
            body["code"] = json!(code);
        }
        if let Some(Value::Object(extra)) = self.extra {
            for (key, value) in extra {
                body[key] = value;
            }
        }
        (self.status, Json(body)).into_response()
    }
}

fn bad(message: impl Into<String>) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, message)
}

fn not_found(message: impl Into<String>) -> AppError {
    AppError::new(StatusCode::NOT_FOUND, message)
}

fn internal(err: anyhow::Error) -> AppError {
    if let Some(limit) = err.downcast_ref::<LimitExceeded>() {
        return limit_error(limit);
    }
    AppError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn limit_error(limit: &LimitExceeded) -> AppError {
    AppError::new(StatusCode::FORBIDDEN, limit.to_string())
        .code("limit_exceeded")
        .extra(json!({"limit": limit.0}))
}

/// 業務上の失敗。上限は 403、「ありません」は 404、それ以外は 400。
fn rejected(err: anyhow::Error) -> AppError {
    if let Some(limit) = err.downcast_ref::<LimitExceeded>() {
        return limit_error(limit);
    }
    let message = err.to_string();
    if message.contains("ありません") || message.contains("見つかりません") {
        not_found(message)
    } else {
        bad(message)
    }
}

/// 本人の会議でなければ、存在しない会議と同じく 404 を返す（他人の会議の有無を知らせない）。
async fn owned(state: &AppState, user: &CurrentUser, meeting_id: i64) -> Result<(), AppError> {
    let conn = state.db().lock().await;
    match db::meeting_owner(&conn, meeting_id).map_err(internal)? {
        Some(Some(owner)) if owner == user.id => Ok(()),
        _ => Err(not_found("会議がありません")),
    }
}

async fn limits_of(state: &AppState, user: &CurrentUser) -> Result<accounts::Limits, AppError> {
    let conn = state.db().lock().await;
    Ok(accounts::plan_for_user(&conn, user.id)
        .map_err(internal)?
        .limits)
}

/// 議題と目安時間（分）を、空の議題を除いて並びをそろえて返す。
fn clean_agenda(body: &MeetingBody) -> Result<(Vec<String>, Vec<Option<i64>>), AppError> {
    let minutes = body.agenda_minutes.clone().unwrap_or_default();
    let mut kept = Vec::new();
    let mut kept_minutes = Vec::new();
    for (index, text) in body.questions.iter().enumerate() {
        if text.trim().is_empty() {
            continue;
        }
        kept.push(text.clone());
        kept_minutes.push(minutes.get(index).copied().flatten());
    }
    Ok((clean_questions(&kept)?, kept_minutes))
}

fn clean_questions(input: &[String]) -> Result<Vec<String>, AppError> {
    let mut questions = Vec::new();
    for body in input {
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let body: String = body.chars().take(500).collect();
        questions.push(body);
    }
    // 件数の上限は設けない（#98）。リクエスト全体の大きさの上限（9MB）が歯止めになる。
    Ok(questions)
}

fn clean_title(title: &str) -> Result<String, AppError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(bad("会議タイトルを入力してください"));
    }
    Ok(title.chars().take(200).collect())
}

fn new_title(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    }
    title.chars().take(200).collect()
}

fn check_fess_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err("FESS の URL は http:// か https:// で始めてください".into())
    }
}

// ---- 設定 ----

/// 音声を Google に送るサーバ認識（Chrome の Web Speech API の既定）を、運用者が許可しているか。
/// 会社の承認が要る外部送信なので、既定では許可しない（#104）。
fn cloud_speech_allowed() -> bool {
    std::env::var("KIKIMIMI_ALLOW_CLOUD_SPEECH").is_ok_and(|value| value.trim() == "1")
}

fn settings_json(
    state: &AppState,
    conn: &rusqlite::Connection,
    user_id: i64,
) -> Result<Value, AppError> {
    let settings = db::get_user_settings(conn, user_id).map_err(internal)?;
    let stats = db::index_stats(conn, user_id).map_err(internal)?;
    let errors = db::index_errors(conn, user_id).map_err(internal)?;
    let uploads = knowledge::list_uploads(&knowledge::uploads_dir(state, user_id));
    Ok(json!({
        "provider": settings.provider,
        "api_key_set": !settings.api_key.is_empty(),
        "openai_key_set": !settings.openai_api_key.is_empty(),
        "anthropic_key_set": !settings.anthropic_api_key.is_empty(),
        "anthropic_model": settings.anthropic_model,
        // Claude を選んだときに文字起こしを任せる提供元（選んでいなくても保存値を返す）。
        "transcriber": if settings.transcriber == "openai" { "openai" } else { "gemini" },
        // リアルタイム文字起こし（#104）: プランで使えるか、サーバ認識（Google に送る）を運用者が許可しているか。
        "realtime_captions": settings.realtime_captions,
        "realtime_allowed": accounts::plan_for_user(conn, user_id).map(|plan| plan.limits.realtime_captions).unwrap_or(false),
        "cloud_speech_allowed": cloud_speech_allowed(),
        "model": settings.model,
        "openai_model": settings.openai_model,
        "openai_transcribe_model": settings.openai_transcribe_model,
        "model_auto": true,
        "model_check": state.model_check(user_id),
        "summary_prompt": settings.summary_prompt,
        "default_summary_prompt": db::DEFAULT_SUMMARY_PROMPT,
        "knowledge_path": settings.knowledge_path,
        "knowledge_root": std::env::var("KIKIMIMI_KNOWLEDGE_ROOT").unwrap_or_default(),
        "fess_url": settings.fess_url,
        "fess_token_set": !settings.fess_token.is_empty(),
        "fess_enabled": settings.fess_enabled,
        "index": stats,
        "index_errors": errors,
        "uploads": uploads
    }))
}

async fn get_settings(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    Ok(Json(settings_json(&state, &conn, user.id)?))
}

#[derive(Deserialize)]
struct SettingsBody {
    provider: Option<String>,
    api_key: Option<String>,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    transcriber: Option<String>,
    realtime_captions: Option<String>,
    summary_prompt: Option<String>,
    knowledge_path: Option<String>,
    fess_url: Option<String>,
    fess_token: Option<String>,
    fess_enabled: Option<bool>,
    #[serde(default)]
    clear_api_key: bool,
    #[serde(default)]
    clear_openai_api_key: bool,
    #[serde(default)]
    clear_anthropic_api_key: bool,
    #[serde(default)]
    clear_fess_token: bool,
}

async fn put_settings(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<SettingsBody>,
) -> Result<Json<Value>, AppError> {
    let current = {
        let conn = state.db().lock().await;
        db::get_user_settings(&conn, user.id).map_err(internal)?
    };
    if let Some(path) = body.knowledge_path.as_deref() {
        if path.trim() != current.knowledge_path {
            knowledge::knowledge_path_allowed(path).map_err(bad)?;
        }
    }
    if let Some(url) = body.fess_url.as_deref() {
        check_fess_url(url).map_err(bad)?;
    }
    if let Some(provider) = body.provider.as_deref() {
        if !matches!(provider, "gemini" | "openai" | "anthropic") {
            return Err(bad("AI の提供元は gemini / openai / anthropic のどれかです"));
        }
    }
    if let Some(mode) = body.realtime_captions.as_deref() {
        match mode {
            "off" => {}
            "local" | "cloud" => {
                let allowed = {
                    let conn = state.db().lock().await;
                    accounts::plan_for_user(&conn, user.id).map_err(internal)?.limits.realtime_captions
                };
                if !allowed {
                    return Err(AppError::new(StatusCode::FORBIDDEN, "リアルタイム文字起こしは、いまのプランでは使えません"));
                }
                if mode == "cloud" && !cloud_speech_allowed() {
                    return Err(bad("音声を Google に送る認識は、運用者が許可していないため選べません"));
                }
            }
            _ => return Err(bad("リアルタイム文字起こしは off / local / cloud のどれかです")),
        }
    }
    // Claude は音声を受け付けないので、文字起こしは Gemini か OpenAI に任せる。
    if let Some(transcriber) = body.transcriber.as_deref() {
        if !matches!(transcriber, "gemini" | "openai") {
            return Err(bad("文字起こしに使う AI は gemini か openai です"));
        }
    }
    let typed = |value: &Option<String>| value.as_ref().is_some_and(|key| !key.trim().is_empty());
    let changed_ai = typed(&body.api_key)
        || typed(&body.openai_api_key)
        || typed(&body.anthropic_api_key)
        || body
            .transcriber
            .as_ref()
            .is_some_and(|transcriber| *transcriber != current.transcriber)
        || body
            .provider
            .as_ref()
            .is_some_and(|provider| *provider != current.provider);
    {
        let conn = state.db().lock().await;
        db::patch_user_settings(
            &conn,
            user.id,
            SettingsPatch {
                api_key: body.api_key,
                summary_prompt: body.summary_prompt,
                knowledge_path: body.knowledge_path,
                fess_url: body.fess_url,
                fess_token: body.fess_token,
                fess_enabled: body.fess_enabled,
                provider: body.provider,
                openai_api_key: body.openai_api_key,
                anthropic_api_key: body.anthropic_api_key,
                transcriber: body.transcriber,
                realtime_captions: body.realtime_captions,
                clear_anthropic_api_key: body.clear_anthropic_api_key,
                clear_api_key: body.clear_api_key,
                clear_openai_api_key: body.clear_openai_api_key,
                clear_fess_token: body.clear_fess_token,
                ..Default::default()
            },
        )
        .map_err(internal)?;
    }
    // 使えるモデルの確認は数十秒かかることがある。保存は待たせず、裏で確かめる。
    if changed_ai {
        jobs::spawn_model_check(state.clone(), user.id);
    }
    let conn = state.db().lock().await;
    Ok(Json(settings_json(&state, &conn, user.id)?))
}

#[derive(Deserialize)]
struct CheckKeyBody {
    provider: Option<String>,
    api_key: Option<String>,
}

async fn check_api_key(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CheckKeyBody>,
) -> Result<Json<Value>, AppError> {
    let provider = body.provider.as_deref().unwrap_or("gemini");
    let typed = body.api_key.unwrap_or_default().trim().to_string();
    let key = if typed.is_empty() {
        let conn = state.db().lock().await;
        let saved = db::get_user_settings(&conn, user.id).map_err(internal)?;
        match provider {
            "openai" => saved.openai_api_key,
            "anthropic" => saved.anthropic_api_key,
            _ => saved.api_key,
        }
    } else {
        typed
    };
    let result = match provider {
        "openai" => crate::openai::check_api_key(state.http(), &key).await,
        "anthropic" => crate::anthropic::check_api_key(state.http(), &key).await,
        _ => crate::gemini::check_api_key(state.http(), &key).await,
    };
    Ok(Json(json!({"ok": result.ok, "message": result.message})))
}

#[derive(Deserialize)]
struct CheckFessBody {
    fess_url: Option<String>,
    fess_token: Option<String>,
}

async fn check_fess(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CheckFessBody>,
) -> Result<Json<Value>, AppError> {
    let saved = {
        let conn = state.db().lock().await;
        db::get_user_settings(&conn, user.id).map_err(internal)?
    };
    let url = body
        .fess_url
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or(saved.fess_url);
    if url.is_empty() {
        return Ok(Json(json!({"ok": false, "message": "FESS の URL を入力してください"})));
    }
    let token = body
        .fess_token
        .filter(|token| !token.trim().is_empty())
        .unwrap_or(saved.fess_token);
    let result = knowledge::check_fess(state.http(), &url, &token).await;
    Ok(Json(match result {
        Ok(message) => json!({"ok": true, "message": message}),
        Err(err) => json!({"ok": false, "message": format!("FESS に接続できません: {err}")}),
    }))
}

// ---- テンプレート ----

async fn list_templates(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    let templates = db::list_templates(&conn, user.id).map_err(internal)?;
    Ok(Json(json!({"templates": templates})))
}

#[derive(Deserialize)]
struct TemplateBody {
    name: String,
    questions: Vec<String>,
}

fn clean_template(body: &TemplateBody) -> Result<(String, Vec<String>), AppError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(bad("テンプレート名を入力してください"));
    }
    let questions = clean_questions(&body.questions)?;
    if questions.is_empty() {
        return Err(bad("質問を1件以上入れてください"));
    }
    Ok((name.chars().take(80).collect(), questions))
}

async fn create_template(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<TemplateBody>,
) -> Result<Json<Value>, AppError> {
    let (name, questions) = clean_template(&body)?;
    let conn = state.db().lock().await;
    let template = db::create_template(&conn, user.id, &name, &questions).map_err(internal)?;
    Ok(Json(json!(template)))
}

async fn update_template(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<TemplateBody>,
) -> Result<Json<Value>, AppError> {
    let (name, questions) = clean_template(&body)?;
    let conn = state.db().lock().await;
    let template = db::update_template(&conn, user.id, id, &name, &questions)
        .map_err(internal)?
        .ok_or_else(|| not_found("テンプレートがありません"))?;
    Ok(Json(json!(template)))
}

async fn delete_template(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    if !db::delete_template(&conn, user.id, id).map_err(internal)? {
        return Err(not_found("テンプレートがありません"));
    }
    Ok(Json(json!({"deleted": true})))
}

// ---- 会議 ----

async fn remove_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    {
        let conn = state.db().lock().await;
        db::delete_meeting(&conn, id).map_err(internal)?;
    }
    let dir = state.data_dir().join("audio").join(id.to_string());
    if dir.is_dir() {
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|err| internal(err.into()))?;
    }
    let docs = knowledge::meeting_docs_dir(&state, id);
    if docs.is_dir() {
        let _ = tokio::fs::remove_dir_all(&docs).await;
    }
    state.publish(id, json!({"type":"deleted"})).await;
    Ok(Json(json!({"deleted": true})))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    folder: String,
}

async fn list_meetings(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    let filter = db::MeetingFilter {
        query: &query.q,
        status: &query.status,
        folder: &query.folder,
    };
    let (total, done, meetings) = db::list_meetings(&conn, user.id, &filter).map_err(internal)?;
    Ok(Json(
        json!({"total": total, "done": done, "meetings": meetings}),
    ))
}

#[derive(Deserialize)]
struct MeetingBody {
    title: String,
    questions: Vec<String>,
    /// 議題ごとの目安時間（分）。questions と同じ並び（空欄は null）。
    #[serde(default)]
    agenda_minutes: Option<Vec<Option<i64>>>,
    #[serde(default)]
    participants: Option<String>,
    /// 新しい会議を入れるフォルダ（作成時だけ使う）。
    #[serde(default)]
    folder_id: Option<i64>,
}

fn clean_participants(text: &str) -> String {
    text.trim().chars().take(500).collect()
}

async fn create_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<MeetingBody>,
) -> Result<Json<Value>, AppError> {
    let title = new_title(&body.title);
    let (questions, minutes) = clean_agenda(&body)?;
    let id = {
        let conn = state.db().lock().await;
        if let Some(folder) = body.folder_id {
            if !db::folder_owned(&conn, user.id, folder).map_err(internal)? {
                return Err(bad("フォルダが見つかりません"));
            }
        }
        let id = db::create_meeting(&conn, user.id, &title, &questions).map_err(internal)?;
        db::set_agenda_minutes(&conn, id, &minutes).map_err(internal)?;
        if body.folder_id.is_some() {
            db::move_meeting(&conn, user.id, id, body.folder_id).map_err(internal)?;
        }
        if let Some(participants) = body.participants.as_deref() {
            db::update_meeting_info(&conn, id, None, Some(&clean_participants(participants)))
                .map_err(internal)?;
        }
        id
    };
    meeting_json(&state, &user, id).await
}

async fn update_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<MeetingBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let title = clean_title(&body.title)?;
    let (questions, minutes) = clean_agenda(&body)?;
    {
        let conn = state.db().lock().await;
        db::replace_draft_questions(&conn, id, &title, &questions).map_err(rejected)?;
        db::set_agenda_minutes(&conn, id, &minutes).map_err(internal)?;
        if let Some(participants) = body.participants.as_deref() {
            db::update_meeting_info(&conn, id, None, Some(&clean_participants(participants)))
                .map_err(rejected)?;
        }
    }
    meeting_json(&state, &user, id).await
}

#[derive(Deserialize)]
struct MeetingPatch {
    title: Option<String>,
    participants: Option<String>,
}

async fn patch_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<MeetingPatch>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let title = match body.title.as_deref() {
        Some(title) => Some(clean_title(title)?),
        None => None,
    };
    let participants = body.participants.as_deref().map(clean_participants);
    {
        let conn = state.db().lock().await;
        db::update_meeting_info(&conn, id, title.as_deref(), participants.as_deref())
            .map_err(rejected)?;
    }
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

async fn get_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    meeting_json(&state, &user, id).await
}

async fn meeting_json(
    state: &AppState,
    user: &CurrentUser,
    id: i64,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    let meeting = db::get_meeting(&conn, id)
        .map_err(internal)?
        .ok_or_else(|| not_found("会議がありません"))?;
    let questions = db::list_questions(&conn, id).map_err(internal)?;
    let segments = db::list_segments(&conn, id).map_err(internal)?;
    let hits = db::list_hits(&conn, id).map_err(internal)?;
    let notes = db::list_notes(&conn, id).map_err(internal)?;
    let docs = db::list_meeting_docs(&conn, id).map_err(internal)?;
    let marks = db::list_marks(&conn, id).map_err(internal)?;
    let errors = db::job_errors(&conn, id).map_err(internal)?;
    let limits = accounts::plan_for_user(&conn, user.id)
        .map_err(internal)?
        .limits;
    let used_ms = accounts::meeting_recorded_ms(&conn, id).map_err(internal)?;
    Ok(Json(json!({
        "meeting": meeting,
        "questions": questions,
        "segments": segments,
        "hits": hits,
        "notes": notes,
        "marks": marks,
        "docs": docs,
        "errors": errors,
        "summarizing": state.is_summarizing(id),
        "limits": {
            "minutes_used": used_ms as f64 / 60_000.0,
            "minutes_limit": limits.meeting_minutes
        }
    })))
}

/// 会議の見出し（タイトル・状態・参加者）が変わったことを、開いている画面に配る。
async fn publish_meeting(state: &AppState, id: i64) {
    let meeting = {
        let conn = state.db().lock().await;
        db::get_meeting(&conn, id).ok().flatten()
    };
    if let Some(meeting) = meeting {
        state
            .publish(id, json!({"type":"meeting","meeting": meeting}))
            .await;
    }
}

#[derive(Deserialize, Default)]
struct StartOpts {
    #[serde(default)]
    speech: String,
    #[serde(default)]
    transcript_only: bool,
}

#[derive(Deserialize, Default)]
struct FinishOpts {
    #[serde(default)]
    close: String,
}

#[derive(Deserialize)]
struct SpeechOpts {
    speech: String,
    #[serde(default)]
    transcript_only: bool,
}

#[derive(Deserialize)]
struct CaptionOpts {
    start_sec: f64,
    text: String,
}

fn parse_or_empty<T: serde::de::DeserializeOwned + Default>(bytes: &Bytes) -> Result<T, AppError> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes).map_err(|_| bad("リクエストを読めません"))
}

async fn start_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let opts: StartOpts = parse_or_empty(&body)?;
    if !opts.speech.is_empty() {
        if opts.speech == "api" {
            require_transcribe_key(&state, user.id).await?;
        }
        let conn = state.db().lock().await;
        db::set_speech(&conn, id, &opts.speech, opts.transcript_only).map_err(rejected)?;
    }
    let limits = limits_of(&state, &user).await?;
    {
        let mut conn = state.db().lock().await;
        db::start_meeting_checked(&mut conn, user.id, id, &limits).map_err(rejected)?;
    }
    jobs::spawn_prefetch(state.clone(), id);
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

async fn pause_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    {
        let conn = state.db().lock().await;
        db::set_paused(&conn, id).map_err(rejected)?;
    }
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

async fn resume_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let limits = limits_of(&state, &user).await?;
    {
        let mut conn = state.db().lock().await;
        db::resume_meeting_checked(&mut conn, user.id, id, &limits).map_err(rejected)?;
    }
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

async fn finish_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let opts: FinishOpts = parse_or_empty(&body)?;
    let mut close = opts.close;
    if close == "api" {
        require_transcribe_key(&state, user.id).await?;
    } else if close == "chrome" {
        let conn = state.db().lock().await;
        let settings = db::get_user_settings(&conn, user.id).map_err(internal)?;
        if !settings.has_chat_key() {
            close = "plain".into();
        }
    }
    {
        let conn = state.db().lock().await;
        db::prepare_finish(&conn, id, &close).map_err(rejected)?;
    }
    state.notify_jobs();
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

async fn require_transcribe_key(state: &AppState, user_id: i64) -> Result<(), AppError> {
    let conn = state.db().lock().await;
    let settings = db::get_user_settings(&conn, user_id).map_err(internal)?;
    if settings.has_transcribe_key() {
        Ok(())
    } else {
        Err(bad("音声認識の確定には、文字起こし用のAPIキーが必要です"))
    }
}

async fn set_speech(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<SpeechOpts>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    if body.speech == "api" {
        require_transcribe_key(&state, user.id).await?;
    }
    {
        let conn = state.db().lock().await;
        db::set_speech(&conn, id, &body.speech, body.transcript_only).map_err(rejected)?;
    }
    publish_meeting(&state, id).await;
    state.notify_jobs();
    meeting_json(&state, &user, id).await
}

async fn save_caption(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<CaptionOpts>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let segment = {
        let conn = state.db().lock().await;
        db::save_caption(&conn, id, body.start_sec, &body.text).map_err(rejected)?
    };
    if let Some(segment) = &segment {
        state
            .publish(id, json!({"type":"segment","segment": segment}))
            .await;
        // 回答・要約用のキーがあれば、書き起こしの文字からアジェンダを更新する（音声は送らない）。
        jobs::spawn_caption_analysis(state.clone(), id);
    }
    Ok(Json(json!({"segment": segment})))
}

async fn cancel_finish(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    {
        let conn = state.db().lock().await;
        db::cancel_finish(&conn, id).map_err(rejected)?;
    }
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

async fn retry_audio(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let released = {
        let conn = state.db().lock().await;
        db::release_retries(&conn, id).map_err(internal)?
    };
    if released > 0 {
        state.notify_jobs();
    }
    jobs::publish_jobs(&state, id).await;
    Ok(Json(json!({"released": released})))
}

async fn give_up_retries(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let released = {
        let conn = state.db().lock().await;
        db::give_up_retries(&conn, id).map_err(internal)?
    };
    state.notify_jobs();
    jobs::publish_jobs(&state, id).await;
    Ok(Json(json!({"released": released})))
}

async fn resummarize(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    {
        let conn = state.db().lock().await;
        let status = db::meeting_status_of(&conn, id).map_err(rejected)?;
        if status != "done" {
            return Err(bad("完了した会議だけ要約を作り直せます"));
        }
        if db::unfinished_jobs(&conn, id).map_err(internal)? > 0 {
            return Err(bad("文字起こしの途中です。終わってから作り直してください"));
        }
    }
    if !jobs::spawn_summary(state.clone(), id) {
        return Err(bad("要約を作成中です"));
    }
    Ok(Json(json!({"started": true})))
}

#[derive(Deserialize)]
struct SummaryBody {
    summary_md: String,
}

async fn put_summary(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<SummaryBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let summary: String = body.summary_md.chars().take(200_000).collect();
    {
        let conn = state.db().lock().await;
        db::update_summary(&conn, id, &summary).map_err(rejected)?;
    }
    publish_meeting(&state, id).await;
    meeting_json(&state, &user, id).await
}

#[derive(Deserialize)]
struct QuestionBody {
    body: String,
    /// 目安時間（分）。
    #[serde(default)]
    minutes: Option<i64>,
}

async fn add_question(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<QuestionBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let text: String = body.body.trim().chars().take(500).collect();
    if text.is_empty() {
        return Err(bad("議題を入力してください"));
    }
    let questions = {
        let conn = state.db().lock().await;
        db::add_question(&conn, id, &text).map_err(rejected)?;
        let mut questions = db::list_questions(&conn, id).map_err(internal)?;
        if let (Some(minutes), Some(last)) = (body.minutes, questions.iter().max_by_key(|q| q.id)) {
            let edit = db::QuestionEdit { body: None, answer: None, status: None, confirmed: None, skip: None, minutes: Some(minutes) };
            db::edit_question(&conn, id, last.id, &edit).map_err(rejected)?;
            questions = db::list_questions(&conn, id).map_err(internal)?;
        }
        questions
    };
    publish_questions(&state, id, &questions).await;
    Ok(Json(json!({"questions": questions})))
}

#[derive(Deserialize)]
struct QuestionPatch {
    body: Option<String>,
    answer: Option<String>,
    status: Option<String>,
    confirmed: Option<bool>,
    /// true:「今回は聞かない」、false: 聞く対象に戻す。
    skip: Option<bool>,
    /// 目安時間（分）。0 で目安なし。
    minutes: Option<i64>,
    /// true: この議題を「いまの議題」にする（#99）。
    #[serde(default)]
    activate: bool,
    /// true: この議題を完了にして、次の議題へ進む（#99）。
    #[serde(default)]
    complete_next: bool,
    /// 操作した時点の録音時間（秒）。経過時間の切り替えに使う。
    at_sec: Option<f64>,
}

async fn edit_question(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, qid)): Path<(i64, i64)>,
    Json(body): Json<QuestionPatch>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let text = body
        .body
        .as_deref()
        .map(|text| text.trim().chars().take(500).collect::<String>());
    if text.as_deref() == Some("") {
        return Err(bad("議題を入力してください"));
    }
    let at = body.at_sec.filter(|sec| sec.is_finite() && *sec >= 0.0);
    let answer = body
        .answer
        .as_deref()
        .map(|text| text.trim().chars().take(2000).collect::<String>());
    let questions = {
        let conn = state.db().lock().await;
        db::edit_question(
            &conn,
            id,
            qid,
            &db::QuestionEdit {
                body: text.as_deref(),
                answer: answer.as_deref(),
                status: body.status.as_deref(),
                confirmed: body.confirmed,
                skip: body.skip,
                minutes: body.minutes,
            },
        )
        .map_err(rejected)?;
        if body.complete_next {
            db::complete_and_next(&conn, id, qid, at).map_err(rejected)?;
        } else if body.activate {
            db::set_active_question(&conn, id, Some(qid), at).map_err(internal)?;
        } else if body.skip == Some(true) {
            // 扱わないことにした議題が「いまの議題」なら外す。
            let active = db::get_meeting(&conn, id).map_err(internal)?.and_then(|m| m.active_question_id);
            if active == Some(qid) {
                db::set_active_question(&conn, id, None, at).map_err(internal)?;
            }
        }
        db::list_questions(&conn, id).map_err(internal)?
    };
    publish_questions(&state, id, &questions).await;
    Ok(Json(json!({"questions": questions})))
}

async fn delete_question(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, qid)): Path<(i64, i64)>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let questions = {
        let conn = state.db().lock().await;
        if !db::delete_question(&conn, id, qid).map_err(internal)? {
            return Err(not_found("質問が見つかりません"));
        }
        db::list_questions(&conn, id).map_err(internal)?
    };
    publish_questions(&state, id, &questions).await;
    Ok(Json(json!({"questions": questions})))
}

async fn publish_questions(state: &AppState, id: i64, questions: &[db::Question]) {
    let active = {
        let conn = state.db().lock().await;
        db::get_meeting(&conn, id)
            .ok()
            .flatten()
            .and_then(|meeting| meeting.active_question_id)
    };
    state
        .publish(
            id,
            json!({"type":"questions","active_question_id": active, "questions": questions}),
        )
        .await;
}

#[derive(Deserialize)]
struct SegmentPatch {
    text: String,
    /// 話者（#83）。空にすると、人が付けた話者を外して推定に戻す。
    #[serde(default)]
    speaker: Option<String>,
}

async fn edit_segment(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, sid)): Path<(i64, i64)>,
    Json(body): Json<SegmentPatch>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let text: String = body.text.trim().chars().take(10_000).collect();
    let segment = {
        let conn = state.db().lock().await;
        db::update_segment(&conn, id, sid, &text, body.speaker.as_deref()).map_err(rejected)?
    };
    state
        .publish(id, json!({"type":"segment","segment": segment, "edited": true}))
        .await;
    Ok(Json(json!({"segment": segment})))
}

// ---- 録音 ----

/// 音声を保存して待ち行列に載せるだけ。外部の AI は呼ばない。
async fn post_audio(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    owned(&state, &user, id).await?;
    let start_ms = headers
        .get("x-start-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| bad("X-Start-Ms が必要です"))?;
    if start_ms < 0 {
        return Err(bad("開始時刻が不正です"));
    }
    if body.len() > 3 * 1024 * 1024 {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "録音区間が大きすぎます",
        ));
    }
    // ブラウザが無音と判定した区間は、録音として残し、文字起こしには回さない。
    let silent = headers
        .get("x-silent")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1");
    let import = headers
        .get("x-import")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1");
    let pcm = audio::decode_wav(&body).map_err(bad)?;
    let duration = audio::duration_sec(&pcm);
    {
        let conn = state.db().lock().await;
        match db::audio_slot(&conn, id, start_ms, import).map_err(internal)? {
            db::AudioSlot::Closed(message) => {
                return Err(AppError::new(StatusCode::CONFLICT, message))
            }
            db::AudioSlot::Duplicate(job_id) => {
                return Ok((
                    StatusCode::ACCEPTED,
                    Json(json!({"accepted": true, "job_id": job_id, "queued": false})),
                ));
            }
            db::AudioSlot::Fresh => {}
        }
    }
    let limits = limits_of(&state, &user).await?;
    let dir = state.data_dir().join("audio").join(id.to_string());
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|err| internal(err.into()))?;
    let path = dir.join(format!("{start_ms}.wav"));
    // 一時ファイルに書き切ってから名前を変える。途中で落ちても中途半端な WAV を残さない。
    // ジョブが無いことは確認済みなので、同じ名前の孤立ファイルは置き換えてよい。
    let tmp = dir.join(format!(".{start_ms}.{}.tmp", uuid::Uuid::new_v4().simple()));
    if let Err(err) = tokio::fs::write(&tmp, &body).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(internal(err.into()));
    }
    if let Err(err) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(internal(err.into()));
    }
    let stored = path.display().to_string();
    let chunk = db::AudioChunk {
        start_ms,
        wav_path: &stored,
        duration_sec: duration,
        bytes: body.len() as i64,
        silent,
        import,
    };
    let enqueued = {
        let mut conn = state.db().lock().await;
        db::enqueue_audio_checked(&mut conn, user.id, id, &chunk, &limits)
    };
    let (job_id, created) = match enqueued {
        Ok(row) => row,
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            if let Some(limit) = err.downcast_ref::<LimitExceeded>() {
                // 受け付け済みの録音はそのまま残す。この区間だけを断り、画面に止め方を知らせる。
                state
                    .publish(id, json!({"type":"limit","limit": limit.0, "reached": true}))
                    .await;
                return Err(limit_error(limit));
            }
            return Err(AppError::new(StatusCode::CONFLICT, err.to_string()));
        }
    };
    if created {
        if !silent {
            state.notify_jobs();
        }
        tracing::info!(
            meeting_id = id,
            start_ms,
            silent,
            "録音区間を受け付けました。文字起こしは後段です"
        );
        jobs::publish_jobs(&state, id).await;
        warn_near_limit(&state, id, &limits).await;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"accepted": true, "job_id": job_id, "queued": created && !silent})),
    ))
}

/// 1 会議の録音時間が残り 5 分を切ったら、画面に知らせる。
async fn warn_near_limit(state: &AppState, id: i64, limits: &accounts::Limits) {
    let Some(limit) = limits.meeting_minutes else {
        return;
    };
    let used = {
        let conn = state.db().lock().await;
        accounts::meeting_recorded_ms(&conn, id).unwrap_or(0)
    };
    if limit * 60_000 - used <= 5 * 60_000 {
        state
            .publish(
                id,
                json!({"type":"limit","reached": false, "limit": {
                    "item": "meeting_minutes", "label": "1 会議の録音時間",
                    "used": used / 60_000, "limit": limit, "unit": "分", "period_end": null
                }}),
            )
            .await;
    }
}

struct AudioPiece {
    path: std::path::PathBuf,
    offset: u64,
    len: u64,
}

/// 会議の録音を 1 本の WAV として返す。区間ファイルを順に読みながら流し、
/// Range に応じて途中から返す（再生位置の移動に使う）。全体をメモリに載せない。
async fn get_audio(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    owned(&state, &user, id).await?;
    let stored = {
        let conn = state.db().lock().await;
        db::chunk_paths(&conn, id).map_err(internal)?
    };
    let mut pieces = Vec::new();
    for stored in &stored {
        let path = db::resolve_audio_path(state.data_dir(), id, stored);
        let Ok(mut file) = tokio::fs::File::open(&path).await else {
            continue;
        };
        let file_len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        let mut head = vec![0u8; 4096.min(file_len as usize)];
        if file.read_exact(&mut head).await.is_err() {
            continue;
        }
        if let Some((offset, len)) = audio::data_span(&head, file_len) {
            pieces.push(AudioPiece { path, offset, len });
        }
    }
    if pieces.is_empty() {
        return Err(not_found("録音がありません"));
    }
    let pcm_len: u64 = pieces.iter().map(|piece| piece.len).sum();
    let mut header_bytes = audio::encode_wav(&[], 16_000);
    header_bytes[4..8].copy_from_slice(&((36 + pcm_len) as u32).to_le_bytes());
    header_bytes[40..44].copy_from_slice(&(pcm_len as u32).to_le_bytes());
    let total = 44 + pcm_len;

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, total));
    let (start, end) = range.unwrap_or((0, total - 1));
    let length = end - start + 1;

    let stream = audio_stream(header_bytes, pieces, start, length);
    let mut response = Response::new(Body::from_stream(stream));
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if range.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).expect("range"),
        );
    }
    Ok(response)
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` の 1 区間だけ扱う。
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (left, right) = spec.split_once('-')?;
    let (start, end) = if left.is_empty() {
        let suffix: u64 = right.parse().ok()?;
        (total.saturating_sub(suffix), total - 1)
    } else {
        let start: u64 = left.parse().ok()?;
        let end = if right.is_empty() {
            total - 1
        } else {
            right.parse::<u64>().ok()?.min(total - 1)
        };
        (start, end)
    };
    (start <= end && start < total).then_some((start, end))
}

fn audio_stream(
    header_bytes: Vec<u8>,
    pieces: Vec<AudioPiece>,
    start: u64,
    length: u64,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    // 仮想的な並び: [WAV ヘッダ 44 バイト][区間 1 の PCM][区間 2 の PCM]...
    struct Cursor {
        header: Vec<u8>,
        pieces: std::vec::IntoIter<AudioPiece>,
        current: Option<(tokio::fs::File, u64)>,
        position: u64,
        piece_start: u64,
        remaining: u64,
    }
    let cursor = Cursor {
        header: header_bytes,
        pieces: pieces.into_iter(),
        current: None,
        position: start,
        piece_start: 44,
        remaining: length,
    };
    futures_util::stream::unfold(cursor, |mut cursor| async move {
        if cursor.remaining == 0 {
            return None;
        }
        if cursor.position < 44 {
            let from = cursor.position as usize;
            let to = (44u64.min(cursor.position + cursor.remaining)) as usize;
            let chunk = Bytes::copy_from_slice(&cursor.header[from..to]);
            cursor.position += chunk.len() as u64;
            cursor.remaining -= chunk.len() as u64;
            return Some((Ok(chunk), cursor));
        }
        loop {
            if let Some((file, left)) = cursor.current.as_mut() {
                if *left > 0 {
                    let want = (*left).min(cursor.remaining).min(64 * 1024) as usize;
                    let mut buf = vec![0u8; want];
                    if let Err(err) = file.read_exact(&mut buf).await {
                        cursor.remaining = 0;
                        return Some((Err(err), cursor));
                    }
                    *left -= want as u64;
                    cursor.position += want as u64;
                    cursor.remaining -= want as u64;
                    return Some((Ok(Bytes::from(buf)), cursor));
                }
                cursor.current = None;
            }
            let piece = cursor.pieces.next()?;
            let piece_end = cursor.piece_start + piece.len;
            if cursor.position >= piece_end {
                cursor.piece_start = piece_end;
                continue;
            }
            let skip = cursor.position - cursor.piece_start;
            cursor.piece_start = piece_end;
            let opened = async {
                let mut file = tokio::fs::File::open(&piece.path).await?;
                file.seek(std::io::SeekFrom::Start(piece.offset + skip))
                    .await?;
                Ok::<_, std::io::Error>(file)
            }
            .await;
            match opened {
                Ok(file) => cursor.current = Some((file, piece.len - skip)),
                Err(err) => {
                    cursor.remaining = 0;
                    return Some((Err(err), cursor));
                }
            }
        }
    })
}

async fn events(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>, AppError>
{
    owned(&state, &user, id).await?;
    let mut inbound = state.subscribe(id).await;
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        // 接続直後に、画面が取り直しの基準にできる合図を送る。
        let _ = tx
            .send(Ok(Event::default().data(json!({"type":"hello"}).to_string())))
            .await;
        loop {
            tokio::select! {
                // 画面が閉じたら、次のイベントを待たずに終える。
                _ = tx.closed() => break,
                received = inbound.recv() => match received {
                    Ok(data) => {
                        if tx.send(Ok(Event::default().data(data))).await.is_err() {
                            break;
                        }
                    }
                    // 取りこぼしたら、画面に全体を取り直させる。
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let resync = json!({"type":"resync"}).to_string();
                        if tx.send(Ok(Event::default().data(resync))).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

#[derive(Deserialize, Default)]
struct ExportQuery {
    #[serde(default)]
    scope: String,
}

async fn export_markdown(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, AppError> {
    owned(&state, &user, id).await?;
    let scope = export::Scope::parse(&query.scope)
        .ok_or_else(|| bad("出力範囲は summary / qa / full です"))?;
    let minutes = {
        let conn = state.db().lock().await;
        export::load_minutes(&conn, id)
            .map_err(internal)?
            .ok_or_else(|| not_found("会議がありません"))?
    };
    let markdown = export::render_scope(&minutes, scope);
    let name = export::file_name(&minutes);
    let disposition = format!(
        "attachment; filename=\"kikimimi.md\"; filename*=UTF-8''{}",
        percent_encode(&name)
    );
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/markdown; charset=utf-8"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).map_err(|err| internal(err.into()))?,
            ),
        ],
        markdown,
    )
        .into_response())
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[derive(Deserialize)]
struct PinBody {
    pinned: bool,
}

async fn pin_hit(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((meeting_id, hit_id)): Path<(i64, i64)>,
    Json(body): Json<PinBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, meeting_id).await?;
    let hits = {
        let conn = state.db().lock().await;
        db::set_pin(&conn, meeting_id, hit_id, body.pinned).map_err(rejected)?;
        db::list_hits(&conn, meeting_id).map_err(internal)?
    };
    state
        .publish(meeting_id, json!({"type":"knowledge","hits": hits}))
        .await;
    Ok(Json(json!({"hits": hits})))
}

// ---- 資料 ----

async fn list_uploads(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Json<Value> {
    Json(json!({"uploads": knowledge::list_uploads(&knowledge::uploads_dir(&state, user.id))}))
}

async fn upload_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let raw = headers
        .get("x-file-name")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| bad("X-File-Name が必要です"))?;
    let name = percent_decode(raw).ok_or_else(|| bad("ファイル名を読めません"))?;
    let dir = knowledge::uploads_dir(&state, user.id);
    knowledge::save_upload(&dir, &name, &body)
        .await
        .map_err(bad)?;
    Ok(Json(json!({"uploads": knowledge::list_uploads(&dir)})))
}

async fn delete_upload(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let dir = knowledge::uploads_dir(&state, user.id);
    knowledge::delete_upload(&dir, &name)
        .await
        .map_err(not_found)?;
    Ok(Json(json!({"uploads": knowledge::list_uploads(&dir)})))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

async fn reindex(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Value>, AppError> {
    let result = knowledge::reindex(&state, user.id)
        .await
        .map_err(|err| bad(err.to_string()))?;
    let conn = state.db().lock().await;
    let errors = db::index_errors(&conn, user.id).map_err(internal)?;
    Ok(Json(json!({
        "docs": result.docs,
        "chunks": result.chunks,
        "errors": result.errors,
        "index_errors": errors
    })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

fn search_terms(q: &str) -> Vec<String> {
    let mut queries = Vec::new();
    crate::gemini::push_query(&mut queries, q);
    if queries.is_empty() && q.trim().chars().count() >= 2 {
        queries.push(q.trim().chars().take(30).collect());
    }
    queries
}

async fn search_knowledge(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, AppError> {
    let (hits, warning) = knowledge::search_all(&state, user.id, &search_terms(&query.q)).await;
    Ok(Json(json!({"hits": hits, "warning": warning})))
}

/// 会議中の手動検索。見つかった資料は会議のナレッジに加え、ピン留めできるようにする。
async fn search_meeting_knowledge(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let terms = search_terms(&query.q);
    if terms.is_empty() {
        return Err(bad("2文字以上で検索してください"));
    }
    let (found, warning) = knowledge::search_for_meeting(&state, user.id, id, &terms).await;
    let hits = {
        let conn = state.db().lock().await;
        db::store_hits(&conn, id, &found).map_err(internal)?;
        db::list_hits(&conn, id).map_err(internal)?
    };
    let matched: Vec<_> = hits
        .iter()
        .filter(|hit| {
            found
                .iter()
                .any(|draft| draft.source == hit.source && draft.locator == hit.locator)
        })
        .cloned()
        .collect();
    state
        .publish(id, json!({"type":"knowledge","hits": hits}))
        .await;
    Ok(Json(json!({"hits": matched, "warning": warning})))
}

// ---- フォルダと並び順 ----

fn clean_folder_name(name: &str) -> Result<String, AppError> {
    let name: String = name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(bad("フォルダ名を入力してください"));
    }
    Ok(name)
}

async fn list_folders(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    let folders = db::list_folders(&conn, user.id).map_err(internal)?;
    let loose: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM meetings WHERE user_id=?1 AND folder_id IS NULL",
            [user.id],
            |r| r.get(0),
        )
        .map_err(|err| internal(err.into()))?;
    Ok(Json(json!({"folders": folders, "unfiled": loose})))
}

#[derive(Deserialize)]
struct FolderBody {
    name: String,
}

async fn create_folder(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<FolderBody>,
) -> Result<Json<Value>, AppError> {
    let name = clean_folder_name(&body.name)?;
    let conn = state.db().lock().await;
    let id = db::create_folder(&conn, user.id, &name).map_err(rejected)?;
    Ok(Json(json!({"id": id, "folders": db::list_folders(&conn, user.id).map_err(internal)?})))
}

async fn rename_folder(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(folder): Path<i64>,
    Json(body): Json<FolderBody>,
) -> Result<Json<Value>, AppError> {
    let name = clean_folder_name(&body.name)?;
    let conn = state.db().lock().await;
    if !db::rename_folder(&conn, user.id, folder, &name).map_err(internal)? {
        return Err(not_found("フォルダが見つかりません"));
    }
    Ok(Json(json!({"folders": db::list_folders(&conn, user.id).map_err(internal)?})))
}

async fn delete_folder(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(folder): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db().lock().await;
    if !db::delete_folder(&conn, user.id, folder).map_err(internal)? {
        return Err(not_found("フォルダが見つかりません"));
    }
    Ok(Json(json!({"folders": db::list_folders(&conn, user.id).map_err(internal)?})))
}

#[derive(Deserialize)]
struct MoveBody {
    folder_id: Option<i64>,
}

async fn move_meeting(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<MoveBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    {
        let conn = state.db().lock().await;
        db::move_meeting(&conn, user.id, id, body.folder_id).map_err(rejected)?;
    }
    meeting_json(&state, &user, id).await
}

#[derive(Deserialize)]
struct OrderBody {
    ids: Vec<i64>,
}

async fn reorder_meetings(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<OrderBody>,
) -> Result<Json<Value>, AppError> {
    if body.ids.is_empty() || body.ids.len() > 2000 {
        return Err(bad("並べ替える会議がありません"));
    }
    let conn = state.db().lock().await;
    db::reorder_meetings(&conn, user.id, &body.ids).map_err(|_| not_found("会議が見つかりません"))?;
    Ok(Json(json!({"ok": true})))
}

/// フォルダ内の会議の議事録を、会議ごとの Markdown にして ZIP 1 つにまとめる。
/// `folder` は数字のフォルダ ID か、"none"（フォルダなし）。下書きは含めない。
async fn export_folder(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(folder): Path<String>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, AppError> {
    let scope = export::Scope::parse(&query.scope)
        .ok_or_else(|| bad("出力範囲は summary / qa / full です"))?;
    let (name, files) = {
        let conn = state.db().lock().await;
        let name = if folder == "none" {
            "フォルダなし".to_string()
        } else {
            let id: i64 = folder.parse().map_err(|_| not_found("フォルダが見つかりません"))?;
            db::list_folders(&conn, user.id)
                .map_err(internal)?
                .into_iter()
                .find(|f| f.id == id)
                .ok_or_else(|| not_found("フォルダが見つかりません"))?
                .name
        };
        let filter = db::MeetingFilter { query: "", status: "", folder: &folder };
        let (_, _, meetings) = db::list_meetings(&conn, user.id, &filter).map_err(internal)?;
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for meeting in meetings.iter().filter(|m| m.status != "draft") {
            let Some(minutes) = export::load_minutes(&conn, meeting.id).map_err(internal)? else {
                continue;
            };
            let mut file = export::file_name(&minutes);
            // 同じ名前が重なったら番号を付ける。
            if files.iter().any(|(existing, _)| existing == &file) {
                file = format!("{}-{}.md", file.trim_end_matches(".md"), meeting.id);
            }
            files.push((file, export::render_scope(&minutes, scope).into_bytes()));
        }
        (name, files)
    };
    if files.is_empty() {
        return Err(not_found("このフォルダに書き出せる会議がありません（下書きは含めません）"));
    }
    let zip = crate::zip::store(&files);
    let file = format!("{}.zip", export::safe_file_stem(&name));
    let disposition = format!(
        "attachment; filename=\"kikimimi.zip\"; filename*=UTF-8''{}",
        percent_encode(&file)
    );
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/zip")),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).map_err(|err| internal(err.into()))?,
            ),
        ],
        zip,
    )
        .into_response())
}

// ---- 取り込み（#96） ----

#[derive(Deserialize)]
struct ImportLine {
    start_sec: f64,
    text: String,
}

#[derive(Deserialize)]
struct ImportTextBody {
    segments: Vec<ImportLine>,
}

/// 文字起こし済みのテキスト（txt / vtt / srt を画面で区間に分けたもの）を書き起こしに入れる。
async fn import_text(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<ImportTextBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    if body.segments.is_empty() {
        return Err(bad("取り込む文がありません"));
    }
    if body.segments.len() > 5000 {
        return Err(bad("一度に取り込めるのは 5000 区間までです"));
    }
    let total: usize = body.segments.iter().map(|line| line.text.chars().count()).sum();
    if total > 500_000 {
        return Err(bad("一度に取り込めるのは 50 万文字までです"));
    }
    let items: Vec<(f64, String)> = body
        .segments
        .iter()
        .map(|line| (line.start_sec, line.text.trim().chars().take(2000).collect()))
        .collect();
    let added = {
        let conn = state.db().lock().await;
        db::import_segments(&conn, id, &items).map_err(rejected)?
    };
    for segment in &added {
        state.publish(id, json!({"type":"segment","segment": segment})).await;
    }
    let count = added.len();
    if count > 0 {
        jobs::spawn_text_import(state.clone(), id, added);
    }
    Ok(Json(json!({"imported": count, "skipped": items.len() - count})))
}

// ---- 顧客資料（#97） ----

async fn list_meeting_docs(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let conn = state.db().lock().await;
    Ok(Json(json!({"docs": db::list_meeting_docs(&conn, id).map_err(internal)?})))
}

/// 顧客から受け取った資料（pdf / docx / xlsx / txt / md）を会議に添付し、本文を索引する。
async fn upload_meeting_doc(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let raw = headers
        .get("x-file-name")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| bad("X-File-Name が必要です"))?;
    let name = percent_decode(raw).ok_or_else(|| bad("ファイル名を読めません"))?;
    let name = knowledge::clean_upload_name(&name).map_err(bad)?;
    if body.is_empty() {
        return Err(bad("ファイルが空です"));
    }
    if body.len() > 8 * 1024 * 1024 {
        return Err(AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "資料は 8MB までです"));
    }
    let ext = std::path::Path::new(&name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    {
        let conn = state.db().lock().await;
        let count = db::list_meeting_docs(&conn, id).map_err(internal)?.len() as i64;
        if count >= db::MAX_MEETING_DOCS {
            return Err(bad(format!("顧客資料は1会議{}件までです", db::MAX_MEETING_DOCS)));
        }
    }
    let dir = knowledge::meeting_docs_dir(&state, id);
    tokio::fs::create_dir_all(&dir).await.map_err(|err| internal(err.into()))?;
    let tmp = dir.join(format!(".{}.{ext}", uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&tmp, &body).await.map_err(|err| internal(err.into()))?;
    // 本文の取り出しは重いことがあるので、別スレッドで行う。
    let path = tmp.clone();
    let extracted = tokio::task::spawn_blocking(move || knowledge::read_text(&path))
        .await
        .map_err(|err| internal(err.into()))?;
    let (chunks, error) = match extracted {
        Ok(text) => {
            let chunks: Vec<(String, String)> = knowledge::chunk_text(&text)
                .into_iter()
                .map(|(heading, body)| (heading, body.trim().to_string()))
                .filter(|(_, body)| body.chars().count() >= 2)
                .collect();
            if chunks.is_empty() {
                (chunks, Some("テキストを抽出できません".to_string()))
            } else {
                (chunks, None)
            }
        }
        Err(err) => (Vec::new(), Some(err)),
    };
    let doc_id = {
        let conn = state.db().lock().await;
        match db::add_meeting_doc(&conn, id, &name, &ext, body.len() as i64, &chunks, error.as_deref()) {
            Ok(doc_id) => doc_id,
            Err(err) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(rejected(err));
            }
        }
    };
    tokio::fs::rename(&tmp, dir.join(format!("{doc_id}.{ext}")))
        .await
        .map_err(|err| internal(err.into()))?;
    // 会議の議題と聞くことで、すぐに一度探して「いまの話題の資料」に出す。
    if error.is_none() {
        jobs::spawn_agenda_search(state.clone(), user.id, id);
    }
    let conn = state.db().lock().await;
    Ok(Json(json!({"docs": db::list_meeting_docs(&conn, id).map_err(internal)?, "error": error})))
}

async fn download_meeting_doc(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, doc_id)): Path<(i64, i64)>,
) -> Result<Response, AppError> {
    owned(&state, &user, id).await?;
    let (file, name) = {
        let conn = state.db().lock().await;
        db::meeting_doc_file(&conn, id, doc_id)
            .map_err(internal)?
            .ok_or_else(|| not_found("資料が見つかりません"))?
    };
    let body = tokio::fs::read(knowledge::meeting_docs_dir(&state, id).join(file))
        .await
        .map_err(|_| not_found("資料の原本が見つかりません"))?;
    let disposition = format!("attachment; filename=\"document\"; filename*=UTF-8''{}", percent_encode(&name));
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream")),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).map_err(|err| internal(err.into()))?,
            ),
        ],
        body,
    )
        .into_response())
}

async fn delete_meeting_doc(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, doc_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let conn = state.db().lock().await;
    let file = db::meeting_doc_file(&conn, id, doc_id)
        .map_err(internal)?
        .ok_or_else(|| not_found("資料が見つかりません"))?
        .0;
    db::delete_meeting_doc(&conn, id, doc_id).map_err(internal)?;
    let _ = std::fs::remove_file(knowledge::meeting_docs_dir(&state, id).join(file));
    Ok(Json(json!({"docs": db::list_meeting_docs(&conn, id).map_err(internal)?})))
}

// ---- 会議中の補助（印・メモ・ここまでの要点） ----

#[derive(Deserialize)]
struct MarkBody {
    at_sec: f64,
}

async fn add_mark(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(body): Json<MarkBody>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    if !body.at_sec.is_finite() {
        return Err(bad("時刻が不正です"));
    }
    let marks = {
        let conn = state.db().lock().await;
        db::add_mark(&conn, id, body.at_sec).map_err(rejected)?;
        db::list_marks(&conn, id).map_err(internal)?
    };
    state.publish(id, json!({"type":"marks","marks": marks})).await;
    Ok(Json(json!({"marks": marks})))
}

async fn delete_mark(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, mark_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let marks = {
        let conn = state.db().lock().await;
        if !db::delete_mark(&conn, id, mark_id).map_err(internal)? {
            return Err(not_found("印が見つかりません"));
        }
        db::list_marks(&conn, id).map_err(internal)?
    };
    state.publish(id, json!({"type":"marks","marks": marks})).await;
    Ok(Json(json!({"marks": marks})))
}

async fn delete_note(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, note_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    let notes = {
        let conn = state.db().lock().await;
        if !db::delete_note(&conn, id, note_id).map_err(internal)? {
            return Err(not_found("メモが見つかりません"));
        }
        db::list_notes(&conn, id).map_err(internal)?
    };
    state.publish(id, json!({"type":"notes","notes": notes})).await;
    Ok(Json(json!({"notes": notes})))
}

async fn recap(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &user, id).await?;
    if state.throttle(&format!("recap:{}", user.id), 10, std::time::Duration::from_secs(600)).is_err() {
        return Err(bad("「ここまでの要点」は 10 分に 10 回までです。少し待ってください"));
    }
    let text = jobs::recap(&state, id).await.map_err(bad)?;
    Ok(Json(json!({"recap": text})))
}

#[cfg(test)]
mod tests;
