use serde_json::Value;

use super::HitDraft;

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
    let url = format!("{}/api/v2/search", base.trim_end_matches('/'));
    let mut request = http
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
        return Err(format!("{status} {}", clip(&body)));
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
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err("URL は http:// か https:// で始めてください".into());
    }
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
        let http = reqwest::Client::new();
        let hits = search_fess(&http, &format!("http://{addr}"), "", &["発注".into()])
            .await
            .unwrap();
        assert_eq!(hits[0].locator, "http://fess.local/a");
    }
}
