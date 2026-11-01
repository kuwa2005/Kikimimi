//! OpenAI を使う場合の文字起こし・回答候補・要約。
//!
//! 音声は音声認識モデル（`/v1/audio/transcriptions`）で文字にし、回答候補と検索語は
//! チャットモデル（`/v1/chat/completions`、JSON 出力）で作る。エラーの文言は
//! `"{HTTP 状態} {メッセージ}"` にそろえ、Gemini と同じ再試行・切り替えの判定を使う。

use std::time::Duration;

use serde_json::{json, Value};

use crate::db::Question;
use crate::gemini::{parse_chunk_json, ChunkAnalysis, KeyCheck};

/// API の接続先。テストでは模擬サーバに向ける（`GIJIROKU_OPENAI_BASE`）。
fn base() -> String {
    std::env::var("GIJIROKU_OPENAI_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into())
}

fn api_error(status: u16, payload: &Value) -> String {
    let message = payload["error"]["message"]
        .as_str()
        .unwrap_or("OpenAI がエラーを返しました");
    let code = payload["error"]["code"].as_str().unwrap_or("");
    if code.is_empty() {
        format!("{status} {message}")
    } else {
        format!("{status} {message} ({code})")
    }
}

fn transport_error(err: &reqwest::Error) -> String {
    let detail = err.to_string().to_ascii_lowercase();
    if detail.contains("timed out") || detail.contains("timeout") {
        "OpenAI が時間内に応答しませんでした".into()
    } else {
        "OpenAI に接続できませんでした".into()
    }
}

async fn read_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|err| format!("OpenAI の応答を読めません: {err}"))?;
    if !status.is_success() {
        return Err(api_error(status.as_u16(), &payload));
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
    let response = match http
        .get(format!("{}/models", base()))
        .bearer_auth(api_key)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return KeyCheck {
                ok: false,
                message: "OpenAI に接続できず、確認できませんでした。".into(),
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
    let response = http
        .get(format!("{}/models", base()))
        .bearer_auth(api_key)
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

fn version_of(name: &str) -> (u32, u32) {
    let digits: Vec<u32> = name
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(2)
        .filter_map(|part| part.parse().ok())
        .collect();
    (digits.first().copied().unwrap_or(0), digits.get(1).copied().unwrap_or(0))
}

/// 日付つきの固定版（`-2025-01-31` など）は、同じ系列の別名より後ろにする。
fn dated(name: &str) -> bool {
    name.split('-')
        .any(|part| part.len() == 4 && part.starts_with("20") && part.chars().all(|ch| ch.is_ascii_digit()))
}

/// 音声認識のモデル。話者分離つきや音声生成のものは除く。精度を優先し mini より通常版を先に。
pub fn rank_transcribe(models: &[String]) -> Vec<String> {
    let mut picked: Vec<&String> = models
        .iter()
        .filter(|id| (id.contains("transcribe") && !id.contains("diarize")) || id.as_str() == "whisper-1")
        .collect();
    picked.sort_by_key(|id| {
        let whisper = id.as_str() == "whisper-1";
        let (major, minor) = version_of(id);
        (
            whisper,
            dated(id),
            std::cmp::Reverse(major),
            std::cmp::Reverse(minor),
            id.contains("mini"),
        )
    });
    picked.into_iter().cloned().collect()
}

/// 回答候補と要約に使うチャットモデル。新しい系列の mini を優先する（速く、費用が小さい）。
pub fn rank_chat(models: &[String]) -> Vec<String> {
    const SKIP: [&str; 12] = [
        "audio", "realtime", "transcribe", "tts", "search", "image", "instruct", "codex", "embedding",
        "moderation", "chat-latest", "nano",
    ];
    let mut picked: Vec<&String> = models
        .iter()
        .filter(|id| id.starts_with("gpt-") && !SKIP.iter().any(|word| id.contains(word)))
        .collect();
    picked.sort_by_key(|id| {
        let (major, minor) = version_of(id);
        (
            dated(id),
            std::cmp::Reverse(major),
            std::cmp::Reverse(minor),
            !id.contains("mini"),
        )
    });
    picked.into_iter().cloned().collect()
}

async fn probe_chat(http: &reqwest::Client, api_key: &str, model: &str) -> Result<(), String> {
    let body = json!({"model": model, "messages": [{"role": "user", "content": "ok"}]});
    let response = http
        .post(format!("{}/chat/completions", base()))
        .bearer_auth(api_key)
        .timeout(Duration::from_secs(15))
        .json(&body)
        .send()
        .await
        .map_err(|err| transport_error(&err))?;
    read_json(response).await.map(|_| ())
}

/// キーで使える「音声認識モデル」と「チャットモデル」を選ぶ。`skip` のモデルは選ばない。
pub async fn choose_models(
    http: &reqwest::Client,
    api_key: &str,
    skip: &str,
) -> Result<(String, String), String> {
    let models = list_models(http, api_key).await?;
    let transcribe = rank_transcribe(&models)
        .into_iter()
        .find(|id| id != skip)
        .ok_or("このキーで使える OpenAI の音声認識モデルが見つかりません")?;
    let mut last = String::from("このキーで使える OpenAI のチャットモデルが見つかりません");
    for model in rank_chat(&models).into_iter().filter(|id| id != skip).take(3) {
        match probe_chat(http, api_key, &model).await {
            Ok(()) => return Ok((transcribe, model)),
            Err(err) if err.starts_with("401") => return Err(err),
            Err(err) => last = err,
        }
    }
    Err(last)
}

pub async fn transcribe(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    wav: &[u8],
    recent: &str,
) -> Result<String, String> {
    let part = reqwest::multipart::Part::bytes(wav.to_vec())
        .file_name("chunk.wav")
        .mime_str("audio/wav")
        .map_err(|err| err.to_string())?;
    // 直近の書き起こしを手がかりに渡すと、固有名詞や文のつながりがそろいやすい。
    let hint: String = recent.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", model.to_string())
        .text("language", "ja")
        .text("response_format", "json");
    if !hint.trim().is_empty() {
        form = form.text("prompt", hint);
    }
    let response = http
        .post(format!("{}/audio/transcriptions", base()))
        .bearer_auth(api_key)
        .timeout(Duration::from_secs(60))
        .multipart(form)
        .send()
        .await
        .map_err(|err| transport_error(&err))?;
    let payload = read_json(response).await?;
    Ok(payload["text"].as_str().unwrap_or("").trim().to_string())
}

async fn chat_json(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
    json_mode: bool,
    timeout: Duration,
) -> Result<String, String> {
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ]
    });
    if json_mode {
        body["response_format"] = json!({"type": "json_object"});
    }
    let response = http
        .post(format!("{}/chat/completions", base()))
        .bearer_auth(api_key)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|err| transport_error(&err))?;
    let payload = read_json(response).await?;
    let text = payload["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if text.trim().is_empty() {
        return Err("OpenAI の応答が空でした".into());
    }
    Ok(text)
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

/// 区間の音声を文字にし、その文で回答候補と検索語を作る。無音なら回答の更新はしない。
#[allow(clippy::too_many_arguments)]
pub async fn analyze_chunk(
    http: &reqwest::Client,
    api_key: &str,
    transcribe_model: &str,
    chat_model: &str,
    wav: &[u8],
    recent: &str,
    questions: &[Question],
) -> Result<ChunkAnalysis, String> {
    let transcript = transcribe(http, api_key, transcribe_model, wav, recent).await?;
    analyze_text(http, api_key, chat_model, transcript, recent, questions).await
}

/// 文字になった区間（音声認識の結果や、取り込んだ書き起こし）から、回答候補と検索語を作る。
pub async fn analyze_text(
    http: &reqwest::Client,
    api_key: &str,
    chat_model: &str,
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
    let system = "あなたは会議支援の係です。会議の発言から、質問への回答候補と社内資料の検索語を作ります。出力は指定の JSON だけです。";
    let user = format!(
        "この区間の発言:\n{transcript}\n\n直近の書き起こし:\n{recent}\n\n質問:\n{}\n\n\
answers には、この区間の発言で触れられた質問だけを入れてください。触れられていない質問は入れないでください。\
candidate はこれまでの回答（current）と今回の発言を合わせた回答候補です。一部だけ分かれば draft、質問に答える内容が揃えば sufficient です。\
evidence には、この区間の発言の中で回答の根拠になった部分をそのまま短く抜き出してください（60文字以内）。無ければ空文字。推測で作らないでください。\
社内資料を探す短い検索語を2件から3件返してください。固有名詞や議題を優先し、各検索語は3文字以上30文字以下です。\
{}\n\
出力: {{\"active_question_id\":null,\"answers\":[{{\"question_id\":0,\"candidate\":\"\",\"status\":\"open\",\"evidence\":\"\"}}],\"search_queries\":[],{}}}",
        questions_text(questions),
        crate::gemini::ASSIST_PROMPT,
        crate::gemini::ASSIST_JSON
    );
    let text = chat_json(http, api_key, chat_model, system, &user, true, Duration::from_secs(40)).await?;
    // 書き起こしは音声認識の結果をそのまま使う。チャットモデルには作らせない。
    let wrapped = match serde_json::from_str::<Value>(&text) {
        Ok(mut value) => {
            value["transcript"] = json!(transcript);
            value.to_string()
        }
        Err(_) => return Err("OpenAI の JSON を読めません".into()),
    };
    let mut analysis = parse_chunk_json(&wrapped)?;
    analysis.transcript = transcript;
    Ok(analysis)
}

pub async fn participants(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    transcript: &str,
) -> Result<Vec<String>, String> {
    let user = format!("書き起こし:\n{transcript}");
    let text = chat_json(
        http,
        api_key,
        model,
        crate::gemini::PARTICIPANTS_PROMPT,
        &user,
        true,
        Duration::from_secs(60),
    )
    .await?;
    Ok(crate::gemini::parse_participants(&text))
}

pub async fn speakers(http: &reqwest::Client, api_key: &str, model: &str, lines: &str) -> Result<Vec<(i64, String)>, String> {
    let text = chat_json(http, api_key, model, crate::gemini::SPEAKERS_PROMPT, lines, true, Duration::from_secs(120)).await?;
    Ok(crate::gemini::parse_speakers(&text))
}

pub async fn summarize(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    context: &str,
) -> Result<String, String> {
    let user = format!("次の材料だけを使い、指定の Markdown を出力してください。\n\n{context}");
    let text = chat_json(http, api_key, model, prompt, &user, false, Duration::from_secs(180)).await?;
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
    fn picks_transcribe_model_before_whisper() {
        let models = ids(&["whisper-1", "gpt-4o-mini-transcribe", "gpt-4o-transcribe", "gpt-4o-transcribe-diarize", "tts-1"]);
        assert_eq!(rank_transcribe(&models), ids(&["gpt-4o-transcribe", "gpt-4o-mini-transcribe", "whisper-1"]));
    }

    #[test]
    fn picks_newest_mini_chat_model() {
        let models = ids(&["gpt-4o", "gpt-4o-mini", "gpt-4.1-mini", "gpt-5-mini", "gpt-5", "gpt-5-mini-2025-08-07", "gpt-4o-audio-preview", "gpt-5-nano", "o3"]);
        let ranked = rank_chat(&models);
        assert_eq!(ranked.first().map(String::as_str), Some("gpt-5-mini"));
        assert!(!ranked.iter().any(|id| id.contains("audio") || id.contains("nano")));
        assert_eq!(ranked.last().map(String::as_str), Some("gpt-5-mini-2025-08-07"));
    }

    #[test]
    fn errors_keep_status_first() {
        let payload = json!({"error": {"message": "Rate limit reached. Please try again in 1.5s.", "code": "rate_limit_exceeded"}});
        let message = api_error(429, &payload);
        assert!(message.starts_with("429 "));
        assert!(crate::gemini::is_transient(&message));
        assert_eq!(crate::gemini::retry_after_secs(&message), Some(2));
        let quota = api_error(429, &json!({"error": {"message": "You exceeded your current quota", "code": "insufficient_quota"}}));
        assert!(!crate::gemini::is_transient(&quota), "残高不足は待っても通らない");
    }
}
