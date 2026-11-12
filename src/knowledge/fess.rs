use serde_json::Value;
use std::sync::OnceLock;

use super::HitDraft;

/// FESS へ接続してよいかを確かめる。会員が入れた URL にサーバから接続するので、
/// 運用者が `KIKIMIMI_FESS_HOSTS`（カンマ区切りのホスト名、または ホスト名:ポート）で許可した宛先だけにする。
/// 許可が無いと、社内の管理画面やクラウドの管理用アドレスへサーバから接続させられる（SSRF）。
pub fn fess_allowed(base: &str) -> Result<(), String> {
    let allowed = std::env::var("KIKIMIMI_FESS_HOSTS").unwrap_or_default();
    allowed_by(base, &allowed)
}

fn allowed_by(base: &str, allowed: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(base.trim()).map_err(|_| "FESS の URL を読めません".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("FESS の URL は http:// か https:// で始めてください".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("FESS の URL に利用者名やパスワードは入れられません。トークン欄を使ってください".into());
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let with_port = url.port_or_known_default().map(|port| format!("{host}:{port}")).unwrap_or_default();
    let list: Vec<String> = allowed
        .split(',')
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect();
    if list.is_empty() {
        return Err("このサーバでは FESS の接続先が許可されていません。運用者に KIKIMIMI_FESS_HOSTS の設定を依頼してください".into());
    }
    if list.iter().any(|item| *item == host || *item == with_port) {
        Ok(())
    } else {
        Err(format!("FESS の接続先 {host} は許可されていません（許可は運用者が KIKIMIMI_FESS_HOSTS で設定します）"))
    }
}

/// FESS 用の接続。リダイレクトはたどらない（許可した宛先から別の宛先へ飛ばされないように）。
fn fess_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("FESS 用の接続を作れません")
    })
}

pub async fn search_fess(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    queries: &[String],
) -> Result<Vec<HitDraft>, String> {
    let mut hits = Vec::new();
    let mut errors = Vec::new();
    for query in queries {
        match search_one(http, base, token, query).await {
            Ok(found) => {
                for hit in found {
                    if hits.len() >= 8 {
                        break;
                    }
                    if hits
                        .iter()
                        .any(|item: &HitDraft| item.locator == hit.locator)
                    {
                        continue;
                    }
                    hits.push(hit);
                }
            }
            Err(err) => errors.push(err),
        }
    }
    if hits.is_empty() {
        if let Some(err) = errors.into_iter().next() {
            return Err(err);
        }
    }
    Ok(hits)
}

async fn search_one(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    query: &str,
) -> Result<Vec<HitDraft>, String> {
    let _ = http;
    fess_allowed(base)?;
    let url = format!("{}/api/v2/search", base.trim_end_matches('/'));
    let mut request = fess_client()
        .get(url)
        .query(&[("q", query), ("num", "5")])
        .timeout(std::time::Duration::from_secs(4));
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|err| format!("{err}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|err| err.to_string())?;
    if !status.is_success() {
        // 相手の応答本文は返さない（許可外の情報を画面に出さない）。
        return Err(format!("FESS が {status} を返しました"));
    }
    parse_fess_search(&body, query)
}

pub fn parse_fess_search(body: &str, query: &str) -> Result<Vec<HitDraft>, String> {
    let value: Value = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let status = value.pointer("/response/status").and_then(|v| v.as_i64());
    if status.is_some() && status != Some(0) {
        let message = value
            .pointer("/response/message")
            .and_then(|v| v.as_str())
            .unwrap_or("FESS が検索に失敗しました");
        return Err(message.to_string());
    }
    let rows = value
        .pointer("/response/data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut hits = Vec::new();
    for row in rows {
        let locator = row["url"].as_str().unwrap_or("").trim().to_string();
        if locator.is_empty() {
            continue;
        }
        let title = decode_entities(row["title"].as_str().unwrap_or("無題").trim());
        let raw = row["content_description"].as_str().unwrap_or("");
        let snippet = clip(&strip_html(raw));
        hits.push(HitDraft {
            source: "fess".into(),
            title,
            locator,
            snippet,
            query: query.to_string(),
        });
    }
    Ok(hits)
}

/// 接続テスト。1 件だけ検索して、応答の形まで確かめる。
pub async fn check_fess(http: &reqwest::Client, base: &str, token: &str) -> Result<String, String> {
    fess_allowed(base)?;
    let hits = search_one(http, base, token, "test").await?;
    Ok(format!("FESS に接続できました（試しの検索で {} 件）", hits.len()))
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

pub fn strip_html(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in input.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            out.push(' ');
            continue;
        }
        if !in_tag {
            out.push(ch);
        }
    }
    decode_entities(&out.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn clip(text: &str) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= 180 {
        return trimmed.to_string();
    }
    trimmed.chars().take(180).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_envelope() {
        let body = r#"{"response":{"status":0,"data":[{"title":"就業規則","url":"http://intra/rules","content_description":"<strong>在宅</strong>勤務の条件"}]}}"#;
        let hits = parse_fess_search(body, "在宅").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "fess");
        assert_eq!(hits[0].title, "就業規則");
        assert!(hits[0].snippet.contains("在宅"));
        assert!(!hits[0].snippet.contains('<'));
        assert_eq!(strip_html("A &amp; B &lt;注&gt;"), "A & B <注>");
    }

    #[tokio::test]
    async fn client_reads_local_fess() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/api/v2/search",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "response": {
                        "status": 0,
                        "data": [{
                            "title": "手順",
                            "url": "http://fess.local/a",
                            "content_description": "発注の手順"
                        }]
                    }
                }))
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // 運用者が許可した宛先にだけ接続する（試験では模擬の FESS を許可する）。
        std::env::set_var("KIKIMIMI_FESS_HOSTS", "127.0.0.1");
        let http = reqwest::Client::new();
        let hits = search_fess(&http, &format!("http://{addr}"), "", &["発注".into()])
            .await
            .unwrap();
        assert_eq!(hits[0].locator, "http://fess.local/a");
    }
}

#[cfg(test)]
mod allow_tests {
    use super::allowed_by;

    #[test]
    fn only_operator_listed_hosts_are_reachable() {
        let list = "fess.example.local, 172.31.0.20:8080";
        assert!(allowed_by("http://fess.example.local:8080", list).is_ok());
        assert!(allowed_by("http://172.31.0.20:8080/", list).is_ok());
        assert!(allowed_by("http://172.31.0.20:9200/", list).is_err(), "許可していないポート");
        assert!(allowed_by("http://169.254.169.254/latest/meta-data", list).is_err());
        assert!(allowed_by("http://127.0.0.1:8765", list).is_err());
        assert!(allowed_by("http://user:pw@fess.example.local", list).is_err());
        assert!(allowed_by("file:///etc/passwd", list).is_err());
        assert!(allowed_by("http://fess.example.local", "").is_err(), "許可が無ければどこにも接続しない");
    }
}
