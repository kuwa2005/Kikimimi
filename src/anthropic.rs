//! Claude（Anthropic API）を使う場合の回答候補・要約・参加者の抽出。
//!
//! Claude は音声を受け付けないため、文字起こしは Gemini か OpenAI に任せ、その文を使って
//! Messages API（`POST /v1/messages`）で回答候補と検索語を作る。エラーの文言は
//! `"{HTTP 状態} Claude: {メッセージ}"` にそろえ、Gemini と同じ再試行・切り替えの判定を使う。

use std::time::Duration;

use serde_json::{json, Value};

use crate::db::Question;
use crate::gemini::{parse_chunk_json, ChunkAnalysis, KeyCheck};

const API_VERSION: &str = "2026-10-21";
/// 既定のモデル。一覧に無いときは同じ系列の新しいものから選ぶ。
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// 安全上の理由で断られたとき、サーバ側で推奨のモデルに引き継がせる（`fallbacks: "default"`）。
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const FALLBACK_MODELS: [&str; 4] = ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

/// API の接続先。テストでは模擬サーバに向ける（`GIJIROKU_ANTHROPIC_BASE`）。
fn base() -> String {
    std::env::var("GIJIROKU_ANTHROPIC_BASE").unwrap_or_else(|_| "https://api.anthropic.com/v1".into())
}

fn request(http: &reqwest::Client, method: reqwest::Method, path: &str, api_key: &str) -> reqwest::RequestBuilder {
    http.request(method, format!("{}{path}", base()))
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
}

/// `retry-after`（秒）があれば、Gemini と同じ「retry in Ns」の形で文言に入れる。
fn api_error(status: u16, payload: &Value, retry_after: Option<&str>) -> String {
    let message = payload["error"]["message"]
        .as_str()
        .unwrap_or("Claude がエラーを返しました");
    let kind = payload["error"]["type"].as_str().unwrap_or("");
    let mut text = if kind.is_empty() {
        format!("{status} Claude: {message}")
    } else {
        format!("{status} Claude: {message} ({kind})")
    };
    if let Some(secs) = retry_after.and_then(|value| value.trim().parse::<f64>().ok()) {
        text.push_str(&format!(" Please retry in {}s", secs.ceil().max(1.0) as u64));
    }
    text
}

fn transport_error(err: &reqwest::Error) -> String {
    let detail = err.to_string().to_ascii_lowercase();
    if detail.contains("timed out") || detail.contains("timeout") {
        "Claude が時間内に応答しませんでした".into()
    } else {
        "Claude に接続できませんでした".into()
    }
}

async fn read_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let payload: Value = response
        .json()
        .await
        .map_err(|err| format!("{} Claude の応答を読めません: {err}", status.as_u16()))?;
    if !status.is_success() {
        return Err(api_error(status.as_u16(), &payload, retry_after.as_deref()));
    }
    Ok(payload)
}

pub async fn check_api_key(http: &reqwest::Client, api_key: &str) -> KeyCheck {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return KeyCheck {
            ok: false,
            message: "確認するAPIキーがありません。入力するか、先に登録してください。".into(),
        };
    }
    let response = match request(http, reqwest::Method::GET, "/models?limit=1", api_key)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return KeyCheck {
                ok: false,
                message: "Claude に接続できず、確認できませんでした。".into(),
            }
        }
    };
    let status = response.status().as_u16();
    match status {
        200..=299 => KeyCheck {
            ok: true,
            message: "APIキーは正しいです。".into(),
        },
        401 | 403 => KeyCheck {
            ok: false,
            message: "APIキーが正しくありません。".into(),
        },
        _ => KeyCheck {
            ok: false,
            message: format!("APIキーを確認できませんでした。（HTTP {status}）"),
        },
    }
}

async fn list_models(http: &reqwest::Client, api_key: &str) -> Result<Vec<String>, String> {
    let response = request(http, reqwest::Method::GET, "/models?limit=1000", api_key)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|err| format!("モデル一覧を取得できません: {}", transport_error(&err)))?;
    let payload = read_json(response).await?;
    Ok(payload["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

fn version_of(id: &str) -> (u32, u32) {
    let digits: Vec<u32> = id
        .split('-')
        .filter(|part| !part.is_empty() && part.len() <= 2 && part.chars().all(|ch| ch.is_ascii_digit()))
        .take(2)
        .filter_map(|part| part.parse().ok())
        .collect();
    (digits.first().copied().unwrap_or(0), digits.get(1).copied().unwrap_or(0))
}

/// 回答候補と要約に使うモデルの順。既定の Opus を先頭にし、Opus → Sonnet → Haiku、新しい版の順。
/// 招待制（Mythos）と最上位（Fable）は、会員が名前で指定したときだけ使う。
pub fn rank_models(models: &[String]) -> Vec<String> {
    let mut picked: Vec<&String> = models
        .iter()
        .filter(|id| id.starts_with("claude-") && !id.contains("mythos") && !id.contains("fable"))
        .collect();
    picked.sort_by_key(|id| {
        let family = if id.contains("opus") {
            0
        } else if id.contains("sonnet") {
            1
        } else if id.contains("haiku") {
            2
        } else {
            3
        };
        let (major, minor) = version_of(id);
        let dated = id.split('-').any(|part| part.len() == 8 && part.starts_with("20"));
        (id.as_str() != DEFAULT_MODEL, family, std::cmp::Reverse(major), std::cmp::Reverse(minor), dated)
    });
    picked.into_iter().cloned().collect()
}

/// キーで使えるモデルを選ぶ。`skip` のモデルは選ばない（使えなかったモデルへ戻さないため）。
/// 試しの呼び出しは費用がかかるので、一覧から選ぶだけにする。
pub async fn choose_model(http: &reqwest::Client, api_key: &str, skip: &str) -> Result<String, String> {
    let models = list_models(http, api_key).await?;
    rank_models(&models)
        .into_iter()
        .find(|id| id != skip)
        .ok_or_else(|| "このキーで使える Claude のモデルが見つかりません".into())
}

/// effort（考える深さ）を受け付ける系列。それより前のモデルには送らない。
fn supports_effort(model: &str) -> bool {
    ["claude-opus-5", "claude-sonnet-5", "claude-haiku-5", "claude-fable-5"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

struct Call<'a> {
    system: &'a str,
    user: String,
    /// 応答の JSON の形（構造化出力）。None なら文章。
    schema: Option<Value>,
    /// 会議中の処理は速さを優先して low にする。None はモデルの既定。
    effort: Option<&'static str>,
    max_tokens: u32,
    timeout: Duration,
}

pub fn message_body(model: &str, call_system: &str, user: &str, schema: Option<&Value>, effort: Option<&str>, max_tokens: u32) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": call_system,
        "messages": [{"role": "user", "content": user}]
    });
    let mut output = serde_json::Map::new();
    if let Some(effort) = effort.filter(|_| supports_effort(model)) {
        output.insert("effort".into(), json!(effort));
    }
    if let Some(schema) = schema {
        output.insert("format".into(), json!({"type": "json_schema", "schema": schema}));
    }
    if !output.is_empty() {
        body["output_config"] = Value::Object(output);
    }
    if FALLBACK_MODELS.contains(&model) {
        body["fallbacks"] = json!("default");
    }
    body
}

/// 応答の文章部分をつなげる。断られた・上限で切れたときはエラーにする。
pub fn response_text(payload: &Value) -> Result<String, String> {
    match payload["stop_reason"].as_str() {
        Some("refusal") => {
            let category = payload["stop_details"]["category"].as_str().unwrap_or("不明");
            return Err(format!("Claude がこの依頼への応答を断りました（分類: {category}）"));
        }
        Some("max_tokens") => return Err("Claude の応答が長さの上限で途中で切れました".into()),
        _ => {}
    }
    let text: String = payload["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    if text.trim().is_empty() {
        return Err("Claude の応答が空でした".into());
    }
    Ok(text)
}

async fn send(http: &reqwest::Client, api_key: &str, model: &str, call: Call<'_>) -> Result<String, String> {
    let body = message_body(model, call.system, &call.user, call.schema.as_ref(), call.effort, call.max_tokens);
    let mut builder = request(http, reqwest::Method::POST, "/messages", api_key).timeout(call.timeout);
    if body.get("fallbacks").is_some() {
        builder = builder.header("anthropic-beta", FALLBACK_BETA);
    }
    let response = builder.json(&body).send().await.map_err(|err| transport_error(&err))?;
    let payload = read_json(response).await?;
    response_text(&payload)
}

fn questions_text(questions: &[Question]) -> String {
    questions
        .iter()
        .map(|q| {
            format!(
                "- id={} status={} body={} current={}",
                q.id,
                q.status,
                q.body,
                if q.answer.is_empty() { "（空）" } else { q.answer.as_str() }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn analysis_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["active_question_id", "answers", "search_queries", "next_question", "new_questions", "skip_questions", "notes"],
        "properties": {
            "active_question_id": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
            "answers": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["question_id", "candidate", "status", "evidence"],
                    "properties": {
                        "question_id": {"type": "integer"},
                        "candidate": {"type": "string"},
                        "status": {"type": "string", "enum": ["open", "draft", "sufficient"]},
                        "evidence": {"type": "string"}
                    }
                }
            },
            "search_queries": {"type": "array", "items": {"type": "string"}},
            "next_question": {
                "type": "object",
                "additionalProperties": false,
                "required": ["text", "angle"],
                "properties": {
                    "text": {"type": "string"},
                    "angle": {"type": "string", "enum": ["具体化", "理由", "数字", "期限", "決裁者", "影響", "次の一歩"]}
                }
            },
            "new_questions": {"type": "array", "items": {"type": "string"}},
            "skip_questions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["question_id", "reason"],
                    "properties": {"question_id": {"type": "integer"}, "reason": {"type": "string"}}
                }
            },
            "notes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "text", "detail"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["decision", "todo", "number", "concern", "term"]},
                        "text": {"type": "string"},
                        "detail": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// 文字起こし済みの区間から、回答候補と検索語を作る。
pub async fn analyze_text(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    transcript: String,
    recent: &str,
    questions: &[Question],
) -> Result<ChunkAnalysis, String> {
    if transcript.is_empty() {
        return Ok(ChunkAnalysis {
            transcript,
            ..Default::default()
        });
    }
    let system = "あなたは会議支援の係です。会議の発言から、質問への回答候補と社内資料の検索語を作ります。";
    let user = format!(
        "<segment>\n{transcript}\n</segment>\n\n<recent>\n{recent}\n</recent>\n\n<questions>\n{}\n</questions>\n\n\
segment はこの区間の発言、recent は直近の書き起こし、questions は会議で聞くことの一覧です。\n\
answers には、この区間の発言で触れられた質問だけを入れてください。触れられていない質問は入れないでください。\
candidate はこれまでの回答（current）と今回の発言を合わせた回答候補です。一部だけ分かれば draft、質問に答える内容が揃えば sufficient です。\
evidence には、この区間の発言の中で回答の根拠になった部分をそのまま短く抜き出してください（60文字以内）。無ければ空文字です。発言に無いことは書かないでください。\
active_question_id は、いま話されている質問の id です。分からなければ null です。\
search_queries には、社内資料を探す短い検索語を2件から3件入れてください。固有名詞や議題を優先し、各検索語は3文字以上30文字以下です。\n{}",
        questions_text(questions),
        crate::gemini::ASSIST_PROMPT
    );
    let text = send(
        http,
        api_key,
        model,
        Call {
            system,
            user,
            schema: Some(analysis_schema()),
            effort: Some("low"),
            max_tokens: 16000,
            timeout: Duration::from_secs(60),
        },
    )
    .await?;
    let mut value: Value = serde_json::from_str(&text).map_err(|_| "Claude の JSON を読めません".to_string())?;
    // 書き起こしは文字起こしの結果をそのまま使う。Claude には作らせない。
    value["transcript"] = json!(transcript);
    let mut analysis = parse_chunk_json(&value.to_string())?;
    analysis.transcript = transcript;
    Ok(analysis)
}

pub async fn participants(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    transcript: &str,
) -> Result<Vec<String>, String> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["participants"],
        "properties": {"participants": {"type": "array", "items": {"type": "string"}}}
    });
    let text = send(
        http,
        api_key,
        model,
        Call {
            system: crate::gemini::PARTICIPANTS_PROMPT,
            user: format!("書き起こし:\n{transcript}"),
            schema: Some(schema),
            effort: Some("low"),
            max_tokens: 16000,
            timeout: Duration::from_secs(120),
        },
    )
    .await?;
    Ok(crate::gemini::parse_participants(&text))
}

pub async fn speakers(http: &reqwest::Client, api_key: &str, model: &str, lines: &str) -> Result<Vec<(i64, String)>, String> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["speakers"],
        "properties": {"speakers": {"type": "array", "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "speaker"],
            "properties": {"id": {"type": "integer"}, "speaker": {"type": "string"}}
        }}}
    });
    let text = send(
        http,
        api_key,
        model,
        Call {
            system: crate::gemini::SPEAKERS_PROMPT,
            user: lines.to_string(),
            schema: Some(schema),
            effort: Some("low"),
            max_tokens: 16000,
            timeout: Duration::from_secs(180),
        },
    )
    .await?;
    Ok(crate::gemini::parse_speakers(&text))
}

pub async fn summarize(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    context: &str,
) -> Result<String, String> {
    let text = send(
        http,
        api_key,
        model,
        Call {
            system: prompt,
            user: format!("次の材料だけを使い、指定の Markdown を出力してください。\n\n{context}"),
            schema: None,
            effort: None,
            max_tokens: 32000,
            timeout: Duration::from_secs(600),
        },
    )
    .await?;
    let cleaned = crate::gemini::strip_code_fence(&text);
    if cleaned.is_empty() {
        return Err("要約が空でした".into());
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn default_model_comes_first() {
        let models = ids(&[
            "claude-haiku-4-5-20251001",
            "claude-sonnet-5-5",
            "claude-opus-4-8",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-haiku-5-5",
        ]);
        let ranked = rank_models(&models);
        assert_eq!(ranked.first().map(String::as_str), Some(DEFAULT_MODEL));
        assert_eq!(ranked.get(1).map(String::as_str), Some("claude-opus-4-8"));
        assert!(!ranked.iter().any(|id| id.contains("fable") || id.contains("mythos")));
        assert_eq!(ranked.last().map(String::as_str), Some("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn body_sets_fallback_effort_and_schema_only_where_supported() {
        let schema = json!({"type": "object"});
        let body = message_body("claude-opus-5-5", "s", "u", Some(&schema), Some("low"), 100);
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert!(body.get("thinking").is_none());
        let old = message_body("claude-haiku-4-5", "s", "u", None, Some("low"), 100);
        assert!(old.get("fallbacks").is_none());
        assert!(old.get("output_config").is_none());
    }

    #[test]
    fn refusal_and_cutoff_are_errors() {
        let refused = json!({"stop_reason": "refusal", "stop_details": {"category": "cyber"}, "content": []});
        assert!(response_text(&refused).unwrap_err().contains("cyber"));
        assert!(response_text(&json!({"stop_reason": "max_tokens", "content": []})).is_err());
        let ok = json!({"stop_reason": "end_turn", "content": [
            {"type": "thinking", "thinking": ""},
            {"type": "text", "text": "前半"},
            {"type": "text", "text": "後半"}
        ]});
        assert_eq!(response_text(&ok).unwrap(), "前半後半");
    }

    #[test]
    fn errors_are_retried_like_other_providers() {
        let limited = api_error(429, &json!({"error": {"type": "rate_limit_error", "message": "Rate limited"}}), Some("7"));
        assert!(limited.starts_with("429 Claude:"));
        assert!(crate::gemini::is_transient(&limited));
        assert_eq!(crate::gemini::retry_after_secs(&limited), Some(7));
        let busy = api_error(529, &json!({"error": {"type": "overloaded_error", "message": "Overloaded"}}), None);
        assert!(crate::gemini::is_transient(&busy));
        let missing = api_error(404, &json!({"error": {"type": "not_found_error", "message": "model: claude-x"}}), None);
        assert!(crate::gemini::should_switch_model(&missing));
        let billing = api_error(402, &json!({"error": {"type": "billing_error", "message": "credit balance is too low"}}), None);
        assert!(!crate::gemini::is_transient(&billing));
    }
}
