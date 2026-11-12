use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

fn test_state() -> AppState {
    let dir = std::env::temp_dir().join(format!("kikimimi-app-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = db::open(&dir.join("t.sqlite")).unwrap();
    AppState::new(conn, dir)
}

/// ログイン済みの利用者（Cookie と CSRF トークン）。
#[derive(Clone)]
struct Client {
    cookie: String,
    csrf: String,
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes)
}

fn json_of(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

fn test_email(login: &str) -> String {
    let trimmed = login.trim().to_ascii_lowercase();
    if trimmed.contains('@') {
        trimmed
    } else {
        format!("{trimmed}@example.com")
    }
}

async fn signup(state: &AppState, login: &str) -> Client {
    let email = test_email(login);
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register/code")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"login_id": email, "password": "password-for-test", "invite": "docomo2"}).to_string(),
        ))
        .unwrap();
    let (status, _, bytes) = send(state, request).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    let code = json_of(&bytes)["code"].as_str().unwrap().to_string();
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"login_id": email, "password": "password-for-test", "code": code, "invite": "docomo2"}).to_string(),
        ))
        .unwrap();
    let (status, headers, bytes) = send(state, request).await;
    assert_eq!(status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&bytes));
    let cookie = headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    Client {
        cookie,
        csrf: json_of(&bytes)["csrf"].as_str().unwrap().to_string(),
    }
}

fn req(client: &Client, method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", &client.cookie)
        .header("x-csrf-token", &client.csrf)
        .header("content-type", "application/json")
}

async fn call(state: &AppState, client: &Client, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let (status, _, bytes) = send(state, req(client, method, uri).body(body).unwrap()).await;
    (status, json_of(&bytes))
}

async fn new_live_meeting(state: &AppState, client: &Client, title: &str) -> i64 {
    let (status, body) = call(
        state,
        client,
        "POST",
        "/api/meetings",
        json!({"title": title, "questions": ["予算は？"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = body["meeting"]["id"].as_i64().unwrap();
    let (status, body) = call(state, client, "POST", &format!("/api/meetings/{id}/start"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    id
}

async fn post_wav(state: &AppState, client: &Client, id: i64, start_ms: i64, wav: Vec<u8>) -> (StatusCode, Value) {
    let request = req(client, "POST", &format!("/api/meetings/{id}/audio"))
        .header("x-start-ms", start_ms.to_string())
        .body(Body::from(wav))
        .unwrap();
    let (status, _, bytes) = send(state, request).await;
    (status, json_of(&bytes))
}

/// テスト用に Free の上限を差し替える。
async fn set_limits(state: &AppState, limits: Value) {
    let conn = state.db().lock().await;
    conn.execute("UPDATE plans SET limits=?1 WHERE code='free'", [limits.to_string()])
        .unwrap();
}

#[tokio::test]
async fn health_needs_no_login_but_api_does() {
    let state = test_state();
    let (status, _, bytes) = send(&state, Request::builder().uri("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], b"ok");
    let (status, _, bytes) = send(&state, Request::builder().uri("/api/meetings").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_of(&bytes)["code"], "unauthenticated");
}

#[tokio::test]
async fn register_login_logout_and_password_change() {
    let state = test_state();
    let alice = signup(&state, "  Alice ").await;
    let (status, me) = call(&state, &alice, "GET", "/api/auth/me", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user"]["login_id"], "alice@example.com");
    assert_eq!(me["user"]["plan"]["code"], "free");

    // 同じメールアドレス（大文字小文字違い）には確認コードを出さない。
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register/code")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "Alice@Example.com", "password": "password-for-test", "plan": "pro", "invite": "docomo2"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json_of(&bytes)["code"], "login_taken");
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register/code")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "ab", "invite": "docomo2"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&bytes)["code"], "invalid_login_id");
    for (login, password, code) in [
        ("bob@example.com", "short", "weak_password"),
        ("carol@example.com", "zzzzzzzzzz", "weak_password"),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/api/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(json!({"login_id": login, "password": password, "code": "000000", "invite": "docomo2"}).to_string()))
            .unwrap();
        let (status, _, bytes) = send(&state, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{login}");
        assert_eq!(json_of(&bytes)["code"], code);
    }

    // 確認コードが違うと登録できない。
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register/code")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "new@example.com", "password": "password-for-test", "invite": "docomo2"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    let sent_body = json_of(&bytes);
    let sent = sent_body["code"].as_str().unwrap().to_string();
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register/verify")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "new@example.com", "code": sent}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    let wrong = if sent == "000000" { "111111" } else { "000000" };
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "new@example.com", "password": "password-for-test", "code": wrong, "invite": "docomo2"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&bytes)["code"], "invalid_code");

    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/register/code")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"login_id": "gate@example.com", "password": "password-for-test", "invite": "nope"}).to_string(),
        ))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&bytes)["code"], "invalid_invite");

    // 誤ったパスワードは断る。正しければ別のセッションが作られる。
    let login = |password: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(json!({"login_id": "alice@example.com", "password": password}).to_string()))
            .unwrap()
    };
    let (status, _, bytes) = send(&state, login("wrong-password!!")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_of(&bytes)["code"], "invalid_credentials");
    let (status, headers, bytes) = send(&state, login("password-for-test")).await;
    assert_eq!(status, StatusCode::OK);
    let second = Client {
        cookie: headers[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string(),
        csrf: json_of(&bytes)["csrf"].as_str().unwrap().to_string(),
    };

    // パスワードを変えると、他のセッションは失効する。
    let (status, _) = call(
        &state,
        &alice,
        "POST",
        "/api/auth/password",
        json!({"current_password": "password-for-test", "new_password": "new-password-123"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(call(&state, &second, "GET", "/api/auth/me", Value::Null).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&state, &alice, "GET", "/api/auth/me", Value::Null).await.0, StatusCode::OK);

    // ログアウトしたセッションは使えない。
    assert_eq!(call(&state, &alice, "POST", "/api/auth/logout", Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&state, &alice, "GET", "/api/auth/me", Value::Null).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn password_reset_mail_changes_the_password() {
    let state = test_state();
    signup(&state, "ada").await;
    let unknown = Request::builder()
        .method("POST")
        .uri("/api/auth/password/forgot")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "missing@example.com"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, unknown).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json_of(&bytes).get("code").is_none());

    let forgot = Request::builder()
        .method("POST")
        .uri("/api/auth/password/forgot")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "ada@example.com"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, forgot).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    let code = json_of(&bytes)["code"].as_str().unwrap().to_string();
    let weak = Request::builder()
        .method("POST")
        .uri("/api/auth/password/reset")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "ada@example.com", "code": code, "password": "short"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, weak).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&bytes)["code"], "weak_password");
    let reset = Request::builder()
        .method("POST")
        .uri("/api/auth/password/reset")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "Ada@Example.com", "code": code, "password": "reset-password-1"}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, reset).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    let old = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "ada@example.com", "password": "password-for-test"}).to_string()))
        .unwrap();
    assert_eq!(send(&state, old).await.0, StatusCode::UNAUTHORIZED);
    let fresh = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({"login_id": "ada@example.com", "password": "reset-password-1"}).to_string()))
        .unwrap();
    assert_eq!(send(&state, fresh).await.0, StatusCode::OK);
}

#[tokio::test]
async fn state_changes_need_the_csrf_token() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/meetings")
        .header("cookie", &alice.cookie)
        .header("content-type", "application/json")
        .body(Body::from(json!({"title": "x", "questions": []}).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json_of(&bytes)["code"], "csrf");
}

#[tokio::test]
async fn login_is_rate_limited() {
    let state = test_state();
    signup(&state, "alice").await;
    let mut last = StatusCode::OK;
    for _ in 0..11 {
        let request = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(json!({"login_id": "alice", "password": "wrong-password!!"}).to_string()))
            .unwrap();
        last = send(&state, request).await.0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn members_cannot_see_each_others_meetings() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let bob = signup(&state, "bob").await;
    let id = new_live_meeting(&state, &alice, "Aの会議").await;
    assert_eq!(post_wav(&state, &alice, id, 0, audio::encode_wav(&[1, 0, 2, 0], 16_000)).await.0, StatusCode::ACCEPTED);
    {
        let conn = state.db().lock().await;
        db::insert_segment(&conn, id, 0.0, "Aの発言").unwrap();
    }
    for (method, uri) in [
        ("GET", format!("/api/meetings/{id}")),
        ("GET", format!("/api/meetings/{id}/audio")),
        ("GET", format!("/api/meetings/{id}/events")),
        ("GET", format!("/api/meetings/{id}/export")),
        ("PATCH", format!("/api/meetings/{id}")),
        ("DELETE", format!("/api/meetings/{id}")),
        ("POST", format!("/api/meetings/{id}/finish")),
        ("POST", format!("/api/meetings/{id}/questions")),
        ("GET", format!("/api/meetings/{id}/knowledge?q=abc")),
    ] {
        let body = if method == "PATCH" {
            json!({"title": "乗っ取り"})
        } else if uri.ends_with("/questions") {
            json!({"body": "x"})
        } else {
            Value::Null
        };
        let (status, _) = call(&state, &bob, method, &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    let (status, _) = post_wav(&state, &bob, id, 8000, audio::encode_wav(&[0, 0], 16_000)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = call(&state, &bob, "GET", "/api/meetings?q=A", Value::Null).await;
    assert_eq!(list["total"], 0);
    assert!(list["meetings"].as_array().unwrap().is_empty());
    let (_, list) = call(&state, &alice, "GET", "/api/meetings", Value::Null).await;
    assert_eq!(list["total"], 1);
    let (status, body) = call(&state, &alice, "GET", &format!("/api/meetings/{id}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["meeting"]["title"], "Aの会議");
}

#[tokio::test]
async fn templates_and_settings_are_personal() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let bob = signup(&state, "bob").await;
    let (status, tpl) = call(&state, &alice, "POST", "/api/templates", json!({"name": "定例", "questions": ["承認者は？"]})).await;
    assert_eq!(status, StatusCode::OK);
    let tid = tpl["id"].as_i64().unwrap();
    let (_, list) = call(&state, &bob, "GET", "/api/templates", Value::Null).await;
    assert!(list["templates"].as_array().unwrap().is_empty());
    assert_eq!(call(&state, &bob, "DELETE", &format!("/api/templates/{tid}"), Value::Null).await.0, StatusCode::NOT_FOUND);

    let (status, saved) = call(
        &state,
        &alice,
        "PUT",
        "/api/settings",
        json!({"provider": "openai", "openai_api_key": "sk-alice-secret", "api_key": "AIza-alice"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["provider"], "openai");
    assert_eq!(saved["openai_key_set"], true);
    assert!(!saved.to_string().contains("sk-alice-secret"), "キーの値は返さない");
    let (_, theirs) = call(&state, &bob, "GET", "/api/settings", Value::Null).await;
    assert_eq!(theirs["api_key_set"], false);
    assert_eq!(theirs["openai_key_set"], false);
    assert_eq!(theirs["provider"], "gemini");
    // キーは暗号化して保存する。
    let conn = state.db().lock().await;
    let stored: String = conn
        .query_row("SELECT openai_api_key FROM user_settings WHERE openai_api_key != ''", [], |r| r.get(0))
        .unwrap();
    assert!(stored.starts_with("v2:") && !stored.contains("sk-alice"));
    drop(conn);
    let (_, cleared) = call(&state, &alice, "PUT", "/api/settings", json!({"clear_openai_api_key": true})).await;
    assert_eq!(cleared["openai_key_set"], false);
    assert_eq!(cleared["api_key_set"], true);
}

#[tokio::test]
async fn uploads_and_index_are_personal() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let bob = signup(&state, "bob").await;
    let request = req(&alice, "POST", "/api/knowledge/files")
        .header("x-file-name", "%E7%99%BA%E6%B3%A8%E3%83%AB%E3%83%BC%E3%83%AB.md")
        .body(Body::from("# 発注ルール\n\n発注は部長承認が必要です。\n"))
        .unwrap();
    let (status, _, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(json_of(&bytes)["uploads"][0]["name"], "発注ルール.md");
    for bad_name in ["..%2Fx.md", "x.exe", ".hidden.md"] {
        let request = req(&alice, "POST", "/api/knowledge/files")
            .header("x-file-name", bad_name)
            .body(Body::from("x"))
            .unwrap();
        assert_eq!(send(&state, request).await.0, StatusCode::BAD_REQUEST, "{bad_name}");
    }
    let (status, body) = call(&state, &alice, "POST", "/api/knowledge/reindex", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["docs"], 1);
    let (_, found) = call(&state, &alice, "GET", "/api/knowledge/search?q=%E7%99%BA%E6%B3%A8%E3%83%AB%E3%83%BC%E3%83%AB", Value::Null).await;
    assert!(!found["hits"].as_array().unwrap().is_empty());
    let (_, found) = call(&state, &bob, "GET", "/api/knowledge/search?q=%E7%99%BA%E6%B3%A8%E3%83%AB%E3%83%BC%E3%83%AB", Value::Null).await;
    assert!(found["hits"].as_array().unwrap().is_empty(), "他人の資料は検索に出ない");
    let (_, bob_settings) = call(&state, &bob, "GET", "/api/settings", Value::Null).await;
    assert_eq!(bob_settings["index"]["docs"], 0);
    assert!(bob_settings["uploads"].as_array().unwrap().is_empty());
    // サーバ上のフォルダは、運用者が許可していなければ指定できない。
    let (status, _) = call(&state, &alice, "PUT", "/api/settings", json!({"knowledge_path": "/etc"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn daily_and_concurrent_limits_are_enforced() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let bob = signup(&state, "bob").await;
    set_limits(&state, json!({"daily_meetings": 2, "meeting_minutes": 60, "storage_bytes": null, "concurrent_recordings": 1})).await;
    let first = new_live_meeting(&state, &alice, "1").await;
    // 1 件録音中なので、2 件目は同時録音数で止まる。
    let (_, created) = call(&state, &alice, "POST", "/api/meetings", json!({"title": "2", "questions": []})).await;
    let second = created["meeting"]["id"].as_i64().unwrap();
    let (status, body) = call(&state, &alice, "POST", &format!("/api/meetings/{second}/start"), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "limit_exceeded");
    assert_eq!(body["limit"]["item"], "concurrent_recordings");
    // 一時停止すれば開始できる。開始の数え直し（同じ会議の再開始）はしない。
    call(&state, &alice, "POST", &format!("/api/meetings/{first}/pause"), Value::Null).await;
    assert_eq!(call(&state, &alice, "POST", &format!("/api/meetings/{second}/start"), Value::Null).await.0, StatusCode::OK);
    assert_eq!(call(&state, &alice, "POST", &format!("/api/meetings/{second}/start"), Value::Null).await.0, StatusCode::OK);
    // 再開は同時録音数で止まる。
    let (status, body) = call(&state, &alice, "POST", &format!("/api/meetings/{first}/resume"), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["limit"]["item"], "concurrent_recordings");
    // 今日 2 件開始したので、3 件目は日の上限で止まる。削除しても数は戻らない。
    call(&state, &alice, "POST", &format!("/api/meetings/{second}/pause"), Value::Null).await;
    call(&state, &alice, "DELETE", &format!("/api/meetings/{second}"), Value::Null).await;
    let (_, created) = call(&state, &alice, "POST", "/api/meetings", json!({"title": "3", "questions": []})).await;
    let third = created["meeting"]["id"].as_i64().unwrap();
    let (status, body) = call(&state, &alice, "POST", &format!("/api/meetings/{third}/start"), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["limit"]["item"], "daily_meetings");
    assert_eq!(body["limit"]["used"], 2);
    // B の枠は A の利用で減らない。
    new_live_meeting(&state, &bob, "B").await;
    let (_, account) = call(&state, &alice, "GET", "/api/account", Value::Null).await;
    let daily = account["usage"].as_array().unwrap().iter().find(|item| item["item"] == "daily_meetings").unwrap().clone();
    assert_eq!(daily["used"], 2);
    assert_eq!(daily["limit"], 2);
    assert!(daily["period_end"].as_str().is_some());
    // 既存の記録は、上限に達しても閲覧・出力できる。
    assert_eq!(call(&state, &alice, "GET", &format!("/api/meetings/{first}"), Value::Null).await.0, StatusCode::OK);
}

#[tokio::test]
async fn recording_minutes_and_storage_are_enforced() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    set_limits(&state, json!({"daily_meetings": null, "meeting_minutes": 1, "storage_bytes": 3_000_000, "concurrent_recordings": null})).await;
    let id = new_live_meeting(&state, &alice, "長い").await;
    let half_minute = audio::encode_wav(&vec![0u8; 16_000 * 2 * 30], 16_000);
    assert_eq!(post_wav(&state, &alice, id, 0, half_minute.clone()).await.0, StatusCode::ACCEPTED);
    assert_eq!(post_wav(&state, &alice, id, 30_000, half_minute.clone()).await.0, StatusCode::ACCEPTED);
    // 同じ区間の再送は二重に数えない。
    let (status, body) = post_wav(&state, &alice, id, 30_000, half_minute.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["queued"], false);
    let (status, body) = post_wav(&state, &alice, id, 60_000, half_minute.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["limit"]["item"], "meeting_minutes");
    assert!(!state.data_dir().join("audio").join(id.to_string()).join("60000.wav").exists(), "断った区間は残さない");
    let (_, detail) = call(&state, &alice, "GET", &format!("/api/meetings/{id}"), Value::Null).await;
    assert_eq!(detail["limits"]["minutes_limit"], 1);
    assert!((detail["limits"]["minutes_used"].as_f64().unwrap() - 1.0).abs() < 0.01);

    // 保存容量は会員の録音の合計で数える（約 0.96MB × 2 を受け付け済み、上限 3MB）。
    let other = new_live_meeting(&state, &alice, "容量").await;
    assert_eq!(post_wav(&state, &alice, other, 0, half_minute.clone()).await.0, StatusCode::ACCEPTED);
    let (status, body) = post_wav(&state, &alice, other, 30_000, half_minute.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["limit"]["item"], "storage_bytes");
}

#[tokio::test]
async fn audio_is_streamed_with_ranges_across_chunks() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let id = new_live_meeting(&state, &alice, "範囲").await;
    let first: Vec<u8> = (0u8..8).collect();
    let second: Vec<u8> = (100u8..108).collect();
    assert_eq!(post_wav(&state, &alice, id, 0, audio::encode_wav(&first, 16_000)).await.0, StatusCode::ACCEPTED);
    assert_eq!(post_wav(&state, &alice, id, 1, audio::encode_wav(&second, 16_000)).await.0, StatusCode::ACCEPTED);
    let (status, _, bytes) = send(&state, req(&alice, "GET", &format!("/api/meetings/{id}/audio")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes.len(), 44 + 16);
    assert_eq!(&bytes[44..52], &first[..]);
    let request = req(&alice, "GET", &format!("/api/meetings/{id}/audio"))
        .header("range", "bytes=50-54")
        .body(Body::empty())
        .unwrap();
    let (status, headers, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 50-54/60");
    assert_eq!(&bytes[..], &[6, 7, 100, 101, 102]);
}

#[tokio::test]
async fn orphan_and_closed_audio() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let id = new_live_meeting(&state, &alice, "孤立").await;
    let dir = state.data_dir().join("audio").join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("0.wav"), b"half written").unwrap();
    let wav = audio::encode_wav(&[1, 0, 2, 0], 16_000);
    let (status, body) = post_wav(&state, &alice, id, 0, wav.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["queued"], true);
    assert_eq!(std::fs::read(dir.join("0.wav")).unwrap(), wav);
    call(&state, &alice, "POST", &format!("/api/meetings/{id}/finish"), Value::Null).await;
    let (status, body) = post_wav(&state, &alice, id, 8000, wav).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("受け付けていません"));
}

#[tokio::test]
async fn editing_and_export_scopes() {
    let state = test_state();
    let alice = signup(&state, "alice").await;
    let id = new_live_meeting(&state, &alice, "仮").await;
    let (status, body) = call(&state, &alice, "PATCH", &format!("/api/meetings/{id}"), json!({"title": "本番の名前", "participants": "田中、佐藤"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["meeting"]["participants"], "田中、佐藤");
    let qid = body["questions"][0]["id"].as_i64().unwrap();
    let (_, body) = call(&state, &alice, "PATCH", &format!("/api/meetings/{id}/questions/{qid}"), json!({"confirmed": true})).await;
    assert_eq!(body["questions"][0]["locked"], true);
    {
        let conn = state.db().lock().await;
        db::insert_segment(&conn, id, 0.0, "全文にだけ出る発言").unwrap();
        db::save_summary(&conn, id, "## 会議の概要\n\n- 決定", None).unwrap();
    }
    for (scope, has_qa, has_transcript) in [("summary", false, false), ("qa", true, false), ("full", true, true)] {
        let request = req(&alice, "GET", &format!("/api/meetings/{id}/export?scope={scope}")).body(Body::empty()).unwrap();
        let (status, headers, bytes) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("- 決定"));
        assert_eq!(text.contains("## アジェンダ"), has_qa, "{scope}");
        assert_eq!(text.contains("全文にだけ出る発言"), has_transcript, "{scope}");
        assert!(headers[header::CONTENT_DISPOSITION].to_str().unwrap().contains(&percent_encode("_本番の名前.md")));
    }
    let request = req(&alice, "GET", &format!("/api/meetings/{id}/export?scope=bogus")).body(Body::empty()).unwrap();
    assert_eq!(send(&state, request).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn legacy_data_is_hidden_until_assigned() {
    let state = test_state();
    {
        let conn = state.db().lock().await;
        conn.execute("INSERT INTO meetings (title, status, created_at) VALUES ('旧データ', 'done', 'x')", [])
            .unwrap();
        db::patch_settings(&conn, SettingsPatch { api_key: Some("AIza-legacy".into()), ..Default::default() }).unwrap();
    }
    let alice = signup(&state, "alice").await;
    let (_, list) = call(&state, &alice, "GET", "/api/meetings", Value::Null).await;
    assert_eq!(list["total"], 0, "初回登録者にも自動では見せない");
    // 会員が先に OpenAI を選んでキーを入れていても、その設定は残す。
    call(&state, &alice, "PUT", "/api/settings", json!({"provider": "openai", "openai_api_key": "sk-mine", "summary_prompt": "私のプロンプト"})).await;
    {
        let conn = state.db().lock().await;
        let user = accounts::user_id_by_login(&conn, &test_email("alice")).unwrap().unwrap();
        db::assign_legacy(&conn, user, state.data_dir()).unwrap();
        // 二度実行しても変わらない。
        db::assign_legacy(&conn, user, state.data_dir()).unwrap();
    }
    let (_, list) = call(&state, &alice, "GET", "/api/meetings", Value::Null).await;
    assert_eq!(list["total"], 1);
    let (_, settings) = call(&state, &alice, "GET", "/api/settings", Value::Null).await;
    assert_eq!(settings["api_key_set"], true, "空だった Gemini のキーは旧設定から入る");
    assert_eq!(settings["provider"], "openai");
    assert_eq!(settings["openai_key_set"], true);
    assert_eq!(settings["summary_prompt"], "私のプロンプト");
}

#[test]
fn range_header_is_parsed() {
    assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
    assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
    assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
    assert_eq!(parse_range("bytes=200-", 100), None);
    assert!(same_secret("abc", "abc"));
    assert!(!same_secret("abc", "abd"));
    assert_eq!(percent_decode("%E7%99%BA.md").as_deref(), Some("発.md"));
}

/// リアルタイム文字起こし（#104）は、プランの機能で使えるかが決まる。サーバ認識は運用者の許可が要る。
#[tokio::test]
async fn realtime_captions_follow_plan_and_operator_policy() {
    let state = test_state();
    let alice = signup(&state, "rtalice").await;
    let (_, settings) = call(&state, &alice, "GET", "/api/settings", Value::Null).await;
    assert_eq!(settings["realtime_captions"], "off");
    assert_eq!(settings["realtime_allowed"], true, "いまは Free でも使える");
    assert_eq!(settings["cloud_speech_allowed"], false);
    let (status, saved) = call(&state, &alice, "PUT", "/api/settings", json!({"realtime_captions": "local"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["realtime_captions"], "local");
    // Google に送る認識は、運用者が許可していなければ選べない。
    let (status, _) = call(&state, &alice, "PUT", "/api/settings", json!({"realtime_captions": "cloud"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, account) = call(&state, &alice, "GET", "/api/account", Value::Null).await;
    assert_eq!(account["plan"]["features"]["realtime_captions"], true);
    // 機能の無いプラン（上位プランを始めたあとの Free を想定）では使えない。
    {
        let conn = state.db().lock().await;
        conn.execute(
            "UPDATE plans SET limits = json_set(limits, '$.realtime_captions', json('false')) WHERE code='free'",
            [],
        )
        .unwrap();
    }
    let (status, _) = call(&state, &alice, "PUT", "/api/settings", json!({"realtime_captions": "local"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, settings) = call(&state, &alice, "GET", "/api/settings", Value::Null).await;
    assert_eq!(settings["realtime_allowed"], false);
}

/// APIキーが無くても Chrome の書き起こしで始められ、終了時に API なしの議事録を選べる。
#[tokio::test]
async fn chrome_speech_starts_without_a_key_and_finishes_plain() {
    let state = test_state();
    let alice = signup(&state, "ears").await;
    let (status, created) = call(
        &state,
        &alice,
        "POST",
        "/api/meetings",
        json!({"title": "キーなし", "questions": ["予算は？"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["meeting"]["id"].as_i64().unwrap();
    let (status, started) = call(
        &state,
        &alice,
        "POST",
        &format!("/api/meetings/{id}/start"),
        json!({"speech": "chrome", "transcript_only": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["meeting"]["speech"], "chrome");
    let (status, caption) = call(
        &state,
        &alice,
        "POST",
        &format!("/api/meetings/{id}/captions"),
        json!({"start_sec": 1.5, "text": "こんにちは"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{caption}");
    assert_eq!(caption["segment"]["text"], "こんにちは");
    let (status, _) = call(
        &state,
        &alice,
        "POST",
        &format!("/api/meetings/{id}/speech"),
        json!({"speech": "api"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, denied) = call(&state, &alice, "POST", &format!("/api/meetings/{id}/finish"), json!({"close": "api"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{denied}");
    assert!(denied["error"].as_str().unwrap_or("").contains("APIキー"), "{denied}");
    let (status, finished) = call(
        &state,
        &alice,
        "POST",
        &format!("/api/meetings/{id}/finish"),
        json!({"close": "plain"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{finished}");
    assert_eq!(finished["meeting"]["finish_requested"], true);
    assert_eq!(finished["meeting"]["finish_close"], "plain");
    assert_eq!(finished["meeting"]["speech"], "chrome");
}
