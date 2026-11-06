use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use crate::db::{AnswerUpdate, Question};

#[derive(Debug, Clone, Default)]
pub struct ChunkAnalysis {
    pub transcript: String,
    pub active_question_id: Option<i64>,
    pub answers: Vec<AnswerUpdate>,
    pub search_queries: Vec<String>,
    /// 次に聞くとよい問いかけ（「[切り口] 問いかけ」の形）。無ければ None。
    pub next_question: Option<String>,
    /// 話の流れから足すとよい聞くこと。
    pub new_questions: Vec<String>,
    /// 不要になった聞くことと、その理由。
    pub skip_questions: Vec<(i64, String)>,
    /// 決定・宿題・数字・懸念・用語のメモ。
    pub notes: Vec<crate::db::NoteDraft>,
}

/// 区間の解析で、回答候補と合わせて作らせる「聞き手の補助」の指示。3 つの提供元で共通。
pub const ASSIST_PROMPT: &str = "あなたは顧客へのヒアリングを支える係でもあります。聞き手が上手に聞けるよう、次も返してください。\
next_question: 未回答の聞くことと話の流れから、聞き手が次に投げかけるとよい問いかけを1つ（50文字以内、話し言葉）。\
相手の課題・現状・理由・数字・期限・決める人・影響を具体化する深掘りを優先します。angle はその切り口で、\
具体化 / 理由 / 数字 / 期限 / 決裁者 / 影響 / 次の一歩 のどれかです。良い問いかけが無ければ text を空文字にします。\
new_questions: 話の流れから新たに確かめるべき重要な事項（既存の聞くことと重ならないもの）を最大1件、短い質問文で。無ければ空の配列。\
skip_questions: 発言で不要になったことがはっきりした聞くこと（例: その件は対象外と言われた）を最大1件、question_id と短い理由で。迷うものは入れません。\
notes: この区間の発言にある、決定事項（decision）・宿題（todo: 誰が・何を・いつまで）・数字や日付（number: 金額・数量・期日など）・\
懸念やリスク（concern）・専門用語や略語（term: text は用語、detail は一般的な意味を40文字以内で）を最大3件。text は60文字以内。\
発言に無いことは書かず、該当が無ければ空の配列にします。";

/// `ASSIST_PROMPT` の出力の形（JSON の例）。
pub const ASSIST_JSON: &str = "\"next_question\":{\"text\":\"\",\"angle\":\"具体化\"},\"new_questions\":[],\"skip_questions\":[{\"question_id\":0,\"reason\":\"\"}],\"notes\":[{\"kind\":\"decision\",\"text\":\"\",\"detail\":\"\"}]";

const ANGLES: [&str; 7] = ["具体化", "理由", "数字", "期限", "決裁者", "影響", "次の一歩"];

/// 解析の JSON から、聞き手の補助の部分を読む。欠けていても失敗にしない。
pub fn parse_assist(value: &Value, analysis: &mut ChunkAnalysis) {
    let text = value["next_question"]["text"].as_str().unwrap_or("").trim();
    if !text.is_empty() {
        let text: String = text.chars().take(80).collect();
        let angle = value["next_question"]["angle"].as_str().unwrap_or("").trim();
        analysis.next_question = Some(if ANGLES.contains(&angle) { format!("[{angle}] {text}") } else { text });
    }
    analysis.new_questions = value["new_questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .take(1)
        .collect();
    analysis.skip_questions = value["skip_questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item["question_id"].as_i64().filter(|id| *id > 0)?;
            Some((id, item["reason"].as_str().unwrap_or("").trim().to_string()))
        })
        .take(1)
        .collect();
    analysis.notes = value["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = item["kind"].as_str()?.trim().to_string();
            let text = item["text"].as_str()?.trim().to_string();
            if text.is_empty() || !crate::db::NOTE_KINDS.contains(&kind.as_str()) {
                return None;
            }
            Some(crate::db::NoteDraft {
                kind,
                text,
                detail: item["detail"].as_str().unwrap_or("").trim().to_string(),
            })
        })
        .take(3)
        .collect();
}

pub struct ListedModel {
    pub name: String,
    pub methods: Vec<String>,
}

/// 一覧のうち、文字起こしに向いた順にモデル名を並べる。
/// Flash を優先し、同じ種類なら安定版、新しい版の順。
fn ranked_names(models: &[ListedModel]) -> Vec<String> {
    let mut ranked: Vec<(Rank, String)> = models
        .iter()
        .filter_map(|model| {
            let name = short_model_name(&model.name);
            rank_key(&name, &model.methods).map(|key| (key, name))
        })
        .collect();
    ranked.sort_by(|left, right| right.0.cmp(&left.0));
    ranked.dedup_by(|left, right| left.1 == right.1);
    ranked.into_iter().map(|(_, name)| name).collect()
}

type Rank = (i32, i32, u32, u32, u32);

pub fn model_rejected(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no longer available")
        || lower.contains("not found")
        || lower.contains("is not supported")
        || lower.contains("not supported")
        || lower.contains("does not support")
        || lower.contains("does not exist")
        || lower.contains("model_not_found")
        || lower.contains("not_found_error")
}

/// モデルが存在しない、または音声を受け付けないときだけ切り替える。
/// 429 や 503 で切り替えると、別モデルの試行で無料枠をさらに消費する。
pub fn should_switch_model(message: &str) -> bool {
    model_rejected(message)
}

/// しばらく待てば同じモデルで通る失敗。
pub fn is_transient(message: &str) -> bool {
    // 残高・支払いの不足（OpenAI の insufficient_quota）は待っても通らない。
    if model_rejected(message) || message.contains("insufficient_quota") {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("503")
        || lower.contains("529")
        || lower.contains("overloaded")
        || lower.contains("quota")
        || lower.contains("high demand")
        || lower.contains("unavailable")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || message.contains("接続できません")
        || message.contains("時間内に応答しませんでした")
}

/// API の接続先。テストでは模擬サーバに向ける（`KIKIMIMI_GEMINI_BASE`）。
pub fn base_url() -> String {
    std::env::var("KIKIMIMI_GEMINI_BASE")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/v1beta".into())
}

/// `Please retry in 19h23m24s` / `594.7ms` の秒数（切り上げ）。無いときは None。
pub fn retry_after_secs(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    let rest = lower
        .split("retry in ")
        .nth(1)
        .or_else(|| lower.split("try again in ").nth(1))?;
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect();
    let mut secs = 0.0;
    let mut saw = false;
    let mut chars = token.chars().peekable();
    while chars.peek().is_some() {
        let mut num = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_digit() || ch == '.' {
                num.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        let mut unit = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_alphabetic() {
                unit.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        if num.is_empty() && unit.is_empty() {
            chars.next();
            continue;
        }
        let value: f64 = num.trim_end_matches('.').parse().unwrap_or(0.0);
        let scale = match unit.as_str() {
            "h" => 3600.0,
            "m" | "min" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => continue,
        };
        secs += value * scale;
        saw = true;
    }
    if !saw || secs <= 0.0 {
        return None;
    }
    Some((secs.ceil() as u64).max(1))
}

/// キーで一覧を取り、実際に応答できるモデルを上から試して1つ決める。
pub struct KeyCheck {
    pub ok: bool,
    pub message: String,
}

/// モデル一覧が取れるかで、キーが正しいかを判定する。
pub fn judge_api_key_response(status: u16, error_message: &str) -> KeyCheck {
    if (200..300).contains(&status) {
        return KeyCheck {
            ok: true,
            message: "APIキーは正しいです。".into(),
        };
    }
    if status == 400 || status == 401 || status == 403 {
        return KeyCheck {
            ok: false,
            message: "APIキーが正しくありません。".into(),
        };
    }
    let detail = error_message.trim();
    let message = if detail.is_empty() {
        format!("APIキーを確認できませんでした。（HTTP {status}）")
    } else {
        format!("APIキーを確認できませんでした。（HTTP {status}）{detail}")
    };
    KeyCheck {
        ok: false,
        message,
    }
}

pub async fn check_api_key(http: &reqwest::Client, api_key: &str) -> KeyCheck {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return KeyCheck {
            ok: false,
            message: "確認するAPIキーがありません。入力するか、先に登録してください。".into(),
        };
    }
    let url = format!("{}/models?pageSize=1", base_url());
    let response = match http.get(url).header("x-goog-api-key", api_key).send().await {
        Ok(response) => response,
        Err(_) => {
            return KeyCheck {
                ok: false,
                message: "Gemini に接続できず、確認できませんでした。".into(),
            };
        }
    };
    let status = response.status().as_u16();
    let payload: Value = response.json().await.unwrap_or(Value::Null);
    let message = payload["error"]["message"].as_str().unwrap_or("");
    judge_api_key_response(status, message)
}

pub async fn choose_model(http: &reqwest::Client, api_key: &str) -> Result<String, String> {
    choose_model_except(http, api_key, "").await
}

/// `skip` に入れたモデルは選ばない。接続に失敗したモデルへ戻さないため。
pub async fn choose_model_except(
    http: &reqwest::Client,
    api_key: &str,
    skip: &str,
) -> Result<String, String> {
    let listed = list_models(http, api_key).await?;
    let ranked = ranked_names(&listed);
    if ranked.is_empty() {
        return Err("このキーで使える Gemini モデルが見つかりません".into());
    }
    let mut last = String::from("別の使えるモデルが見つかりません");
    let mut busy = None;
    // 試行の呼び出しも無料枠を使うので、上位の数件だけ試す。
    for name in ranked.into_iter().take(3) {
        if name == skip {
            continue;
        }
        match probe_model(http, api_key, &name).await {
            Ok(Probe::Ready) => return Ok(name),
            Ok(Probe::Busy) => {
                if busy.is_none() {
                    busy = Some(name);
                }
            }
            Ok(Probe::Next(message)) => last = message,
            Err(err) => return Err(err),
        }
    }
    if let Some(name) = busy {
        return Ok(name);
    }
    Err(last)
}

fn rank_key(name: &str, methods: &[String]) -> Option<Rank> {
    if !methods.iter().any(|method| method == "generateContent") {
        return None;
    }
    let tier = model_tier(name)?;
    let (major, minor, patch) = version_tuple(name);
    Some((tier, stability(name), major, minor, patch))
}

fn short_model_name(name: &str) -> String {
    name.trim()
        .strip_prefix("models/")
        .unwrap_or(name.trim())
        .to_string()
}

fn model_tier(name: &str) -> Option<i32> {
    let lower = name.to_ascii_lowercase();
    if !lower.contains("gemini") || blocked_model(&lower) {
        return None;
    }
    if lower.contains("flash") && !lower.contains("lite") {
        Some(3)
    } else if lower.contains("flash") {
        Some(2)
    } else if lower.contains("pro") {
        Some(1)
    } else {
        Some(0)
    }
}

fn blocked_model(lower: &str) -> bool {
    const BLOCKED: [&str; 12] = [
        "embedding",
        "imagen",
        "veo",
        "aqa",
        "tts",
        "live",
        "gemma",
        "robot",
        "computer-use",
        "learnlm",
        "native-audio",
        "image",
    ];
    BLOCKED.iter().any(|word| lower.contains(word))
}

fn stability(name: &str) -> i32 {
    let lower = name.to_ascii_lowercase();
    if lower.contains("exp") || lower.contains("preview") {
        0
    } else {
        1
    }
}

fn version_tuple(name: &str) -> (u32, u32, u32) {
    let mut nums = [0u32; 3];
    let mut slot = 0;
    let bytes = name.as_bytes();
    let mut index = 0;
    while index < bytes.len() && slot < 3 {
        if bytes[index].is_ascii_digit() {
            let mut value = 0u32;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                value = value
                    .saturating_mul(10)
                    .saturating_add(u32::from(bytes[index] - b'0'));
                index += 1;
            }
            nums[slot] = value;
            slot += 1;
        } else {
            index += 1;
        }
    }
    (nums[0], nums[1], nums[2])
}

enum Probe {
    Ready,
    Busy,
    Next(String),
}

async fn list_models(http: &reqwest::Client, api_key: &str) -> Result<Vec<ListedModel>, String> {
    let mut page: Option<String> = None;
    let mut out = Vec::new();
    for _ in 0..5 {
        let mut url = reqwest::Url::parse(&format!("{}/models", base_url()))
            .map_err(|err| err.to_string())?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("pageSize", "100");
            if let Some(token) = page.as_deref() {
                query.append_pair("pageToken", token);
            }
        }
        let response = http
            .get(url)
            .header("x-goog-api-key", api_key)
            .send()
            .await
            .map_err(|err| format!("モデル一覧を取得できません: {err}"))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|err| format!("モデル一覧を読めません: {err}"))?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &payload));
        }
        let models = payload["models"].as_array().cloned().unwrap_or_default();
        for model in models {
            let name = model["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let methods = model["supportedGenerationMethods"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            out.push(ListedModel { name, methods });
        }
        page = payload["nextPageToken"]
            .as_str()
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        if page.is_none() {
            break;
        }
    }
    Ok(out)
}

async fn probe_model(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
) -> Result<Probe, String> {
    let model = safe_model(model);
    let url =
        format!("{}/models/{model}:generateContent", base_url());
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "ok"}]}],
        "generationConfig": {"temperature": 0, "maxOutputTokens": 8}
    });
    let response = match http
        .post(url)
        .timeout(Duration::from_secs(6))
        .header("x-goog-api-key", api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(Probe::Busy),
    };
    let status = response.status();
    let payload: Value = response.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(Probe::Ready);
    }
    if status.as_u16() == 429 || status.as_u16() == 503 {
        return Ok(Probe::Busy);
    }
    let message = api_error(status.as_u16(), &payload);
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(message);
    }
    if status.as_u16() == 404 || model_rejected(&message) {
        return Ok(Probe::Next(message));
    }
    if status.as_u16() == 400 {
        return Ok(Probe::Ready);
    }
    Ok(Probe::Next(message))
}

fn gemini_transport_error(err: &reqwest::Error) -> String {
    let detail = err.to_string().to_ascii_lowercase();
    if detail.contains("timed out") || detail.contains("timeout") {
        "Gemini が時間内に応答しませんでした".into()
    } else {
        "Gemini に接続できませんでした".into()
    }
}

fn api_error(status: u16, payload: &Value) -> String {
    let message = payload["error"]["message"]
        .as_str()
        .unwrap_or("Gemini がエラーを返しました");
    format!("{status} {message}")
}

pub fn safe_model(model: &str) -> String {
    let trimmed = model.trim();
    let ok = !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if ok {
        trimmed.to_string()
    } else {
        crate::db::DEFAULT_MODEL.to_string()
    }
}

pub async fn analyze_chunk(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    wav: &[u8],
    recent: &str,
    questions: &[Question],
) -> Result<ChunkAnalysis, String> {
    let questions_text = questions
        .iter()
        .map(|q| {
            format!(
                "- id={} status={} body={} current={}",
                q.id,
                q.status,
                q.body,
                if q.answer.is_empty() {
                    "（空）"
                } else {
                    q.answer.as_str()
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "あなたは会議支援の書き起こし係です。入力音声を日本語で文字起こししてください。無音や判別不能なら transcript は空文字です。\
話者の言いよどみは除き、意味が分かる文にしてください。\
answers には、この音声の発言で触れられた質問だけを入れてください。触れられていない質問は answers に入れないでください。\
candidate はこれまでの回答（current）と今回の発言を合わせた回答候補です。一部だけ分かれば draft、質問に答える内容が揃えば sufficient です。\
evidence には、この音声の中で回答の根拠になった発言をそのまま短く抜き出してください（60文字以内）。根拠になる発言が無ければ空文字にしてください。推測で作らないでください。\
社内資料を探す短い検索語を2件から3件返してください。固有名詞や議題を優先し、各検索語は3文字以上30文字以下です。\
{ASSIST_PROMPT}\
出力は次の JSON だけです。\n\
{{\"transcript\":\"\",\"active_question_id\":null,\"answers\":[{{\"question_id\":0,\"candidate\":\"\",\"status\":\"open\",\"evidence\":\"\"}}],\"search_queries\":[],{ASSIST_JSON}}}\n\n\
直近の書き起こし:\n{recent}\n\n質問:\n{questions_text}"
    );
    let options = GenerateOptions {
        json: true,
        quick: true,
        timeout: Duration::from_secs(30),
    };
    let text = match generate(http, api_key, model, &prompt, Some(wav), &options).await {
        // 思考を止められないモデルは、指定を外してもう一度だけ送る。
        Err(err) if thinking_rejected(&err) => {
            let options = GenerateOptions {
                quick: false,
                ..options
            };
            generate(http, api_key, model, &prompt, Some(wav), &options).await?
        }
        other => other?,
    };
    parse_chunk_json(&text)
}

/// 取り込んだ書き起こし（文字）から、回答候補・検索語・聞き手の補助を作る。書き起こしは渡した文のまま。
pub async fn analyze_text(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    transcript: String,
    recent: &str,
    questions: &[Question],
) -> Result<ChunkAnalysis, String> {
    if transcript.trim().is_empty() {
        return Ok(ChunkAnalysis { transcript, ..Default::default() });
    }
    let questions_text = questions
        .iter()
        .map(|q| format!("- id={} status={} body={} current={}", q.id, q.status, q.body, if q.answer.is_empty() { "（空）" } else { q.answer.as_str() }))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "あなたは会議支援の係です。次の「この区間の発言」（取り込んだ書き起こし）から、質問への回答候補と社内資料の検索語を作ります。\
answers には、この区間の発言で触れられた質問だけを入れてください。触れられていない質問は answers に入れないでください。\
candidate はこれまでの回答（current）と今回の発言を合わせた回答候補です。一部だけ分かれば draft、質問に答える内容が揃えば sufficient です。\
evidence には、この区間の発言の中で回答の根拠になった部分をそのまま短く抜き出してください（60文字以内）。無ければ空文字。推測で作らないでください。\
社内資料を探す短い検索語を2件から3件返してください。固有名詞や議題を優先し、各検索語は3文字以上30文字以下です。\
{ASSIST_PROMPT}\
出力は次の JSON だけです。\n\
{{\"active_question_id\":null,\"answers\":[{{\"question_id\":0,\"candidate\":\"\",\"status\":\"open\",\"evidence\":\"\"}}],\"search_queries\":[],{ASSIST_JSON}}}\n\n\
この区間の発言:\n{transcript}\n\n直近の書き起こし:\n{recent}\n\n質問:\n{questions_text}"
    );
    let options = GenerateOptions {
        json: true,
        quick: false,
        timeout: Duration::from_secs(60),
    };
    let text = generate(http, api_key, model, &prompt, None, &options).await?;
    let raw = strip_code_fence(&text);
    let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else {
        return Err("JSON がありません".into());
    };
    if end < start {
        return Err("JSON の括弧が対応していません".into());
    }
    let mut value: Value = serde_json::from_str(&raw[start..=end]).map_err(|err| err.to_string())?;
    value["transcript"] = Value::String(transcript.clone());
    let mut analysis = parse_chunk_json(&value.to_string())?;
    analysis.transcript = transcript;
    Ok(analysis)
}

/// 文字起こしだけを行う（回答は Claude など別の AI が作るとき）。無音や判別不能なら空文字。
pub async fn transcribe(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    wav: &[u8],
    recent: &str,
) -> Result<String, String> {
    let prompt = format!(
        "入力音声を日本語で文字起こししてください。無音や判別不能なら transcript は空文字です。\
話者の言いよどみは除き、意味が分かる文にしてください。直近の書き起こしは、固有名詞や文のつながりの手がかりです。\
出力は次の JSON だけです。{{\"transcript\":\"\"}}\n\n直近の書き起こし:\n{recent}"
    );
    let options = GenerateOptions {
        json: true,
        quick: true,
        timeout: Duration::from_secs(30),
    };
    let text = match generate(http, api_key, model, &prompt, Some(wav), &options).await {
        Err(err) if thinking_rejected(&err) => {
            let options = GenerateOptions {
                quick: false,
                ..options
            };
            generate(http, api_key, model, &prompt, Some(wav), &options).await?
        }
        other => other?,
    };
    let raw = strip_code_fence(&text);
    let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else {
        return Err("JSON がありません".into());
    };
    if end < start {
        return Err("JSON の括弧が対応していません".into());
    }
    let value: Value = serde_json::from_str(&raw[start..=end]).map_err(|err| err.to_string())?;
    Ok(value["transcript"]
        .as_str()
        .ok_or("transcript がありません")?
        .trim()
        .to_string())
}

/// 話者を推定するときの指示（#83）。会議の終了後に、書き起こしの発言ごとに話者を付ける。
pub const SPEAKERS_PROMPT: &str = "次の会議の書き起こしの各発言（#番号つき）について、話している人を推定してください。\
参加者の一覧があれば、その名前を使ってください。名乗り・呼びかけ・話の流れ（質問と答え、役割）から判断します。\
名前が分からない人は「話者A」「話者B」のように、同じ人には同じ呼び名を一貫して使ってください。\
「これまでの呼び名」があれば、同じ人には同じ呼び名を使ってください。判断できない発言は speaker を空文字にします。推測で新しい名前を作らないでください。\
出力は次の JSON だけです。{\"speakers\":[{\"id\":0,\"speaker\":\"\"}]}";

/// 話者の推定の JSON を読む（#83）。
pub fn parse_speakers(text: &str) -> Vec<(i64, String)> {
    let raw = strip_code_fence(text);
    let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let value: Value = serde_json::from_str(&raw[start..=end]).unwrap_or(Value::Null);
    value["speakers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item["id"].as_i64()?;
            let mut speaker = item["speaker"].as_str()?.trim().to_string();
            for suffix in ["さん", "様", "氏"] {
                if let Some(stripped) = speaker.strip_suffix(suffix) {
                    speaker = stripped.trim().to_string();
                }
            }
            let speaker: String = speaker.chars().take(30).collect();
            (!speaker.is_empty()).then_some((id, speaker))
        })
        .collect()
}

pub async fn speakers(http: &reqwest::Client, api_key: &str, model: &str, lines: &str) -> Result<Vec<(i64, String)>, String> {
    let prompt = format!("{SPEAKERS_PROMPT}\n\n{lines}");
    let options = GenerateOptions {
        json: true,
        quick: false,
        timeout: Duration::from_secs(120),
    };
    let text = generate(http, api_key, model, &prompt, None, &options).await?;
    Ok(parse_speakers(&text))
}

/// 参加者を拾うときの指示。発言に出てきた名前だけを返させ、推測で作らせない。
pub const PARTICIPANTS_PROMPT: &str = "次の会議の書き起こしから、会議の参加者の名前を拾ってください。\
名乗り（「〇〇です」）、自己紹介、呼びかけ（「〇〇さん、どうですか」）など、発言の中で参加者として出てきた名前だけを対象にします。\
話題に出ただけの社外の人物や会社名、役職だけの呼び方（部長、皆さん など）は含めないでください。推測で名前を作らないでください。\
敬称（さん、様、くん、氏 など）は外してください。見つからなければ空の配列にしてください。\
出力は次の JSON だけです。{\"participants\":[\"名前\"]}";

/// 参加者を拾った JSON を読み、名前を整える（敬称を外し、重複と長すぎるものを除く）。
pub fn parse_participants(text: &str) -> Vec<String> {
    let raw = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```");
    let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let value: Value = serde_json::from_str(&raw[start..=end]).unwrap_or(Value::Null);
    let mut names: Vec<String> = Vec::new();
    for item in value["participants"].as_array().into_iter().flatten() {
        let Some(name) = item.as_str() else { continue };
        let mut name = name.trim().to_string();
        for suffix in ["さん", "様", "さま", "くん", "君", "ちゃん", "氏", "殿"] {
            if let Some(stripped) = name.strip_suffix(suffix) {
                name = stripped.trim().to_string();
            }
        }
        let count = name.chars().count();
        if count == 0 || count > 20 || names.contains(&name) {
            continue;
        }
        names.push(name);
        if names.len() >= 30 {
            break;
        }
    }
    names
}

pub async fn participants(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    transcript: &str,
) -> Result<Vec<String>, String> {
    let prompt = format!("{PARTICIPANTS_PROMPT}\n\n書き起こし:\n{transcript}");
    let options = GenerateOptions {
        json: true,
        quick: false,
        timeout: Duration::from_secs(60),
    };
    let text = generate(http, api_key, model, &prompt, None, &options).await?;
    Ok(parse_participants(&text))
}

pub async fn summarize(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    context: &str,
) -> Result<String, String> {
    let instruction = format!(
        "{prompt}\n\n次の材料だけを使い、指定の Markdown を出力してください。\n\n{context}"
    );
    let options = GenerateOptions {
        json: false,
        quick: false,
        timeout: Duration::from_secs(180),
    };
    let text = generate(http, api_key, model, &instruction, None, &options).await?;
    let cleaned = strip_code_fence(&text);
    if cleaned.is_empty() {
        return Err("要約が空でした".into());
    }
    Ok(cleaned)
}

/// 応答全体を囲む ```markdown … ``` を外す。本文中のコードブロックは残す。
pub fn strip_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(body) = rest.strip_suffix("```") {
            let body = match body.find('\n') {
                Some(pos) if !body[..pos].contains(' ') => &body[pos + 1..],
                _ => body,
            };
            return body.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn thinking_rejected(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.starts_with("400") && (lower.contains("thinking") || lower.contains("budget"))
}

#[derive(Clone, Copy)]
struct GenerateOptions {
    json: bool,
    /// 文字起こしのように推論が要らない呼び出しは、思考を止めて応答を早める。
    quick: bool,
    timeout: Duration,
}

async fn generate(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    wav: Option<&[u8]>,
    options: &GenerateOptions,
) -> Result<String, String> {
    let model = safe_model(model);
    let url =
        format!("{}/models/{model}:generateContent", base_url());
    let mut parts = vec![Part {
        text: Some(prompt.to_string()),
        inline_data: None,
    }];
    if let Some(wav) = wav {
        parts.push(Part {
            text: None,
            inline_data: Some(InlineData {
                mime_type: "audio/wav".into(),
                data: base64::engine::general_purpose::STANDARD.encode(wav),
            }),
        });
    }
    let mut body = serde_json::to_value(GeminiRequest {
        contents: vec![Content {
            role: Some("user".into()),
            parts,
        }],
        generation_config: GenConfig {
            temperature: 0.2,
            response_mime_type: "application/json".into(),
        },
    })
    .map_err(|err| err.to_string())?;
    if !options.json {
        body.as_object_mut()
            .and_then(|obj| obj.remove("generationConfig"));
        body["generationConfig"] = serde_json::json!({"temperature": 0.2});
    }
    if options.quick {
        body["generationConfig"]["thinkingConfig"] = serde_json::json!({"thinkingBudget": 0});
    }

    let response = http
        .post(url)
        .timeout(options.timeout)
        .header("x-goog-api-key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|err| gemini_transport_error(&err))?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|err| format!("Gemini の応答を読めません: {err}"))?;
    if !status.is_success() {
        return Err(api_error(status.as_u16(), &payload));
    }
    let parts = payload["candidates"][0]["content"]["parts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let text = parts
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err("Gemini の応答が空でした".into());
    }
    Ok(text)
}

pub fn parse_chunk_json(text: &str) -> Result<ChunkAnalysis, String> {
    let raw = text.trim();
    let raw = raw
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let start = raw.find('{').ok_or("JSON がありません")?;
    let end = raw.rfind('}').ok_or("JSON がありません")?;
    if end < start {
        return Err("JSON の括弧が対応していません".into());
    }
    let value: Value = serde_json::from_str(&raw[start..=end]).map_err(|err| err.to_string())?;
    let transcript = value["transcript"]
        .as_str()
        .ok_or("transcript がありません")?
        .trim()
        .to_string();
    let active_question_id = value["active_question_id"].as_i64();
    let mut answers = Vec::new();
    if let Some(rows) = value["answers"].as_array() {
        for row in rows {
            let Some(question_id) = row["question_id"].as_i64() else {
                continue;
            };
            if question_id <= 0 {
                continue;
            }
            answers.push(AnswerUpdate {
                question_id,
                candidate: row["candidate"].as_str().unwrap_or("").trim().to_string(),
                status: row["status"].as_str().unwrap_or("open").trim().to_string(),
                evidence: row["evidence"].as_str().unwrap_or("").trim().to_string(),
            });
        }
    }
    let mut search_queries = Vec::new();
    if let Some(rows) = value["search_queries"].as_array() {
        for row in rows {
            if let Some(query) = row.as_str() {
                push_query(&mut search_queries, query);
            }
        }
    }
    let mut analysis = ChunkAnalysis {
        transcript,
        active_question_id,
        answers,
        search_queries,
        ..Default::default()
    };
    parse_assist(&value, &mut analysis);
    Ok(analysis)
}

pub fn push_query(out: &mut Vec<String>, query: &str) {
    let trimmed = query.trim();
    let count = trimmed.chars().count();
    if !(3..=30).contains(&count) {
        return;
    }
    if out.iter().any(|existing| existing == trimmed) {
        return;
    }
    if out.len() >= 3 {
        return;
    }
    out.push(trimmed.to_string());
}

pub fn queries_from_agenda(title: &str, questions: &[Question]) -> Vec<String> {
    let mut loose = Vec::new();
    consider(&mut loose, title);
    for question in questions.iter().take(8) {
        consider(&mut loose, &question.body);
    }
    loose.truncate(8);
    loose
}

fn consider(out: &mut Vec<String>, text: &str) {
    let trimmed: String = text.trim().chars().take(30).collect();
    if trimmed.chars().count() < 3 {
        return;
    }
    if out.iter().any(|existing| existing == &trimmed) {
        return;
    }
    out.push(trimmed);
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenConfig,
}

#[derive(Serialize)]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<InlineData>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InlineData {
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenConfig {
    temperature: f32,
    response_mime_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_are_parsed_without_honorifics() {
        let parsed = parse_speakers("```json\n{\"speakers\":[{\"id\":3,\"speaker\":\"田中さん\"},{\"id\":4,\"speaker\":\"\"},{\"id\":5,\"speaker\":\"話者A\"}]}\n```");
        assert_eq!(parsed, [(3, "田中".to_string()), (5, "話者A".to_string())]);
    }

    #[test]
    fn assist_fields_are_read_and_bounded() {
        let analysis = parse_chunk_json(
            r#"{"transcript":"t","answers":[],"search_queries":[],
               "next_question":{"text":"いつまでに決めたいですか","angle":"期限"},
               "new_questions":["決裁者は誰か","二件目"],
               "skip_questions":[{"question_id":3,"reason":"対象外"},{"question_id":0,"reason":""}],
               "notes":[{"kind":"todo","text":"見積を送る","detail":""},{"kind":"bad","text":"x"},{"kind":"term","text":"PoC","detail":"概念実証"}]}"#,
        )
        .unwrap();
        assert_eq!(analysis.next_question.as_deref(), Some("[期限] いつまでに決めたいですか"));
        assert_eq!(analysis.new_questions, ["決裁者は誰か"]);
        assert_eq!(analysis.skip_questions, [(3, "対象外".to_string())]);
        assert_eq!(analysis.notes.len(), 2);
        // 補助の欄が無い古い形でも読める。
        let plain = parse_chunk_json(r#"{"transcript":"t","answers":[],"search_queries":[]}"#).unwrap();
        assert!(plain.next_question.is_none() && plain.notes.is_empty());
    }

    #[test]
    fn retry_after_reads_milliseconds_and_compound_units() {
        assert_eq!(retry_after_secs("Please retry in 594.749449ms."), Some(1));
        assert_eq!(retry_after_secs("Please retry in 17.360946429s."), Some(18));
        assert_eq!(retry_after_secs("Please retry in 19h23m24s."), Some(69_804));
        assert_eq!(retry_after_secs("Please retry in 2m."), Some(120));
        assert_eq!(retry_after_secs("no hint"), None);
    }

    #[test]
    fn summary_fence_is_removed_but_inner_code_stays() {
        assert_eq!(strip_code_fence("```markdown\n## 概要\n- a\n```"), "## 概要\n- a");
        assert_eq!(strip_code_fence("```\n## 概要\n```"), "## 概要");
        let inner = "## 手順\n```\nls\n```\n以上";
        assert_eq!(strip_code_fence(inner), inner);
    }

    #[test]
    fn participant_names_are_cleaned() {
        let names = parse_participants("```json\n{\"participants\":[\"田中さん\",\"佐藤 様\",\"田中\",\"\",\"あ\"]}\n```");
        assert_eq!(names, vec!["田中".to_string(), "佐藤".to_string(), "あ".to_string()]);
        assert!(parse_participants("not json").is_empty());
    }

    #[test]
    fn parses_fenced_json() {
        let text = "```json\n{\"transcript\":\"予算の話\",\"active_question_id\":2,\"answers\":[{\"question_id\":2,\"candidate\":\"来期は増額\",\"status\":\"draft\"}],\"search_queries\":[\"来期予算\",\"あ\"]}\n```";
        let parsed = parse_chunk_json(text).unwrap();
        assert_eq!(parsed.transcript, "予算の話");
        assert_eq!(parsed.active_question_id, Some(2));
        assert_eq!(parsed.answers.len(), 1);
        assert_eq!(parsed.search_queries, vec!["来期予算".to_string()]);
    }

    #[test]
    fn broken_json_is_an_error_and_does_not_panic() {
        let result = std::panic::catch_unwind(|| parse_chunk_json("} invalid {"));
        let err = result.expect("括弧の逆順で止まらない").expect_err("不正な JSON");
        assert!(err.contains("括弧"));
        assert!(parse_chunk_json("{}").is_err());
        assert!(parse_chunk_json("{\"transcript\":null}").is_err());
        let silent = parse_chunk_json("{\"transcript\":\"\"}").unwrap();
        assert_eq!(silent.transcript, "");
    }

    fn listed(name: &str) -> ListedModel {
        ListedModel {
            name: name.into(),
            methods: vec!["generateContent".into(), "countTokens".into()],
        }
    }

    #[test]
    fn picks_newest_flash_and_skips_unusable() {
        let models = vec![
            listed("models/gemini-2.5-flash"),
            listed("models/gemini-2.5-flash-lite"),
            listed("models/gemini-3.8-flash"),
            listed("models/gemini-3.5-pro"),
            listed("models/text-embedding-004"),
            listed("models/gemini-embedding-001"),
            ListedModel {
                name: "models/gemini-3.9-flash".into(),
                methods: vec!["countTokens".into()],
            },
        ];
        assert_eq!(ranked_names(&models).first().map(String::as_str), Some("gemini-3.8-flash"));
    }

    #[test]
    fn prefers_stable_flash_over_newer_preview() {
        let models = vec![
            listed("models/gemini-3.8-flash"),
            listed("models/gemini-3.9-flash-preview"),
        ];
        assert_eq!(ranked_names(&models).first().map(String::as_str), Some("gemini-3.8-flash"));
    }

    #[test]
    fn keeps_the_same_model_when_the_quota_is_full() {
        let quota = "429 You exceeded your current quota. Quota exceeded for metric: generate_content_free_tier_requests, model: gemini-3.8-flash. Please retry in 19h23m24.8s.";
        assert!(!should_switch_model(quota));
        assert!(is_transient(quota));
        assert_eq!(retry_after_secs(quota), Some(19 * 3600 + 23 * 60 + 25));
        assert!(should_switch_model(
            "models/gemini-x is not found or is not supported"
        ));
        assert!(!is_transient("APIキーが正しくありません"));
    }

    #[test]
    fn judges_api_key_without_calling_gemini() {
        assert!(judge_api_key_response(200, "").ok);
        assert!(!judge_api_key_response(400, "API key not valid").ok);
        assert_eq!(
            judge_api_key_response(403, "unregistered").message,
            "APIキーが正しくありません。"
        );
        assert!(judge_api_key_response(503, "high demand")
            .message
            .contains("503"));
    }

    #[test]
    fn falls_back_to_pro_when_flash_is_absent() {
        let models = vec![listed("models/gemini-3.5-pro")];
        assert_eq!(ranked_names(&models).first().map(String::as_str), Some("gemini-3.5-pro"));
    }
}
