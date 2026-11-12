//! 録音の後段。
//!
//! マイクが止まらないよう、ここは音声の受付とは別タスクで動く。
//! 区間の文字起こしと回答候補は数件ずつ並行して進め、ナレッジ検索と要約は
//! 別タスクに回す。ある区間の解析が遅くても、次の区間と録音の受付は待たない。

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::anthropic;
use crate::db;
use crate::export::{self, Minutes};
use crate::gemini;
use crate::knowledge;
use crate::openai;
use crate::state::{AppState, ModelCheck};

/// 起動直後のモデル確認と、古い失敗区間の再武装は、プロセスで一度だけ。
static STARTED: AtomicBool = AtomicBool::new(false);

fn workers() -> usize {
    std::env::var("KIKIMIMI_WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|n: &usize| (1..=8).contains(n))
        .unwrap_or(2)
}

/// モデルの切り替えは、その会員の設定だけを変える。他の会員のモデルには触れない。
async fn replace_model(
    state: &AppState,
    user_id: i64,
    api_key: &str,
    failed: &str,
) -> Result<String, String> {
    let next = gemini::choose_model_except(state.http(), api_key, failed).await?;
    if next == failed {
        return Err("別の使えるモデルが見つかりません".into());
    }
    let conn = state.db().lock().await;
    db::patch_user_settings(
        &conn,
        user_id,
        db::SettingsPatch {
            model: Some(next.clone()),
            ..Default::default()
        },
    )
    .map_err(|err| err.to_string())?;
    Ok(next)
}

async fn requeue_failed_audio(state: &AppState) {
    let jobs = {
        let conn = state.db().lock().await;
        match db::list_failed_jobs(&conn) {
            Ok(jobs) => jobs,
            Err(err) => {
                tracing::warn!("失敗した録音の読み直しを飛ばします: {err:#}");
                return;
            }
        }
    };
    let mut armed = 0;
    for job in jobs {
        if job.attempts >= db::MAX_ATTEMPTS || !gemini::is_transient(&job.error) {
            continue;
        }
        let path = db::resolve_audio_path(state.data_dir(), job.meeting_id, &job.wav_path);
        if !path.is_file() {
            continue;
        }
        let wait = gemini::retry_after_secs(&job.error).unwrap_or(60);
        let when = db::stamp_after(wait);
        let conn = state.db().lock().await;
        if db::arm_retry(&conn, job.id, &when).is_ok() {
            armed += 1;
        }
    }
    if armed > 0 {
        tracing::info!("失敗していた録音 {armed} 件は、待ち時間のあと再試行します");
    }
}

/// その会員のキーで使えるモデルを選び直す。設定の保存時に、裏で一度だけ走らせる。
pub fn spawn_model_check(state: AppState, user_id: i64) {
    if cfg!(test) {
        return;
    }
    if state.model_check(user_id).running {
        return;
    }
    state.set_model_check(
        user_id,
        ModelCheck {
            running: true,
            error: None,
        },
    );
    tokio::spawn(async move {
        let error = refresh_model(&state, user_id).await.err();
        state.set_model_check(
            user_id,
            ModelCheck {
                running: false,
                error,
            },
        );
    });
}

async fn refresh_model(state: &AppState, user_id: i64) -> Result<(), String> {
    let settings = {
        let conn = state.db().lock().await;
        db::get_user_settings(&conn, user_id).map_err(|err| format!("設定を読めません: {err:#}"))?
    };
    if settings.uses_anthropic() && !settings.anthropic_api_key.is_empty() {
        let model = save_anthropic_model(state, user_id, &settings.anthropic_api_key, "").await?;
        tracing::info!(user_id, "回答・要約に使う Claude のモデルは {model} です");
    }
    // 文字起こしに使う提供元（Claude を選んだときは transcriber）のモデルを選ぶ。
    if settings.transcribe_provider() == "openai" {
        if settings.openai_api_key.is_empty() {
            return Ok(());
        }
        let (transcribe, chat) = save_openai_models(state, user_id, &settings.openai_api_key, "").await?;
        tracing::info!(user_id, "OpenAI のモデルは {transcribe} / {chat} です");
        return Ok(());
    }
    if settings.api_key.is_empty() {
        return Ok(());
    }
    let model = gemini::choose_model(state.http(), &settings.api_key)
        .await
        .map_err(|err| {
            tracing::warn!("モデルの自動選択に失敗しました: {err}");
            err
        })?;
    let conn = state.db().lock().await;
    db::patch_user_settings(
        &conn,
        user_id,
        db::SettingsPatch {
            model: Some(model.clone()),
            ..Default::default()
        },
    )
    .map_err(|err| format!("使用モデルを保存できません: {err:#}"))?;
    tracing::info!(user_id, "文字起こしに使うモデルは {model} です");
    Ok(())
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            let worker = tokio::spawn(run_worker(state.clone()));
            match worker.await {
                Ok(()) => tracing::error!("文字起こし係が終了したため、やり直します"),
                Err(err) => tracing::error!("文字起こし係が止まったため、やり直します: {err}"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

async fn run_worker(state: AppState) {
    {
        let conn = state.db().lock().await;
        if let Err(err) = db::reset_inflight(&conn) {
            tracing::error!("未完了ジョブの復旧に失敗しました: {err:#}");
        }
    }
    if !STARTED.swap(true, Ordering::SeqCst) {
        requeue_failed_audio(&state).await;
    }
    let mut last_sweep = std::time::Instant::now();
    let limit = Arc::new(Semaphore::new(workers()));
    let mut running = JoinSet::new();
    loop {
        if last_sweep.elapsed() >= Duration::from_secs(60) {
            last_sweep = std::time::Instant::now();
            sweep_stale(&state).await;
        }
        while let Some(done) = running.try_join_next() {
            if let Err(err) = done {
                tracing::error!("区間の処理が異常終了しました: {err}");
            }
        }
        if let Ok(permit) = limit.clone().try_acquire_owned() {
            let job = {
                let mut conn = state.db().lock().await;
                match db::claim_pending(&mut conn) {
                    Ok(Some(job)) => Some(job),
                    Ok(None) => db::claim_due_retry(&mut conn).ok().flatten(),
                    Err(err) => {
                        tracing::error!("待ち行列の取得に失敗しました: {err:#}");
                        None
                    }
                }
            };
            if let Some(job) = job {
                let state = state.clone();
                running.spawn(async move {
                    let _permit = permit;
                    run_guarded(&state, job).await;
                });
                continue;
            }
        }
        let finishing = {
            let mut conn = state.db().lock().await;
            db::claim_finish(&mut conn).ok().flatten()
        };
        if let Some(meeting_id) = finishing {
            spawn_summary(state.clone(), meeting_id);
            continue;
        }
        tokio::select! {
            _ = state.wait_jobs() => {}
            _ = running.join_next(), if !running.is_empty() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

/// 録音中のまま 15 分以上音声が届かない会議は、タブを閉じたなどとみなして一時停止にする。
/// 同時に録音できる会議の枠を占め続けないようにするため。
async fn sweep_stale(state: &AppState) {
    let paused = {
        let conn = state.db().lock().await;
        let _ = crate::accounts::purge_expired_sessions(&conn);
        db::pause_stale(&conn, 15).unwrap_or_default()
    };
    for meeting_id in paused {
        tracing::info!(meeting_id, "音声が届かないため、会議を一時停止にしました");
        let meeting = {
            let conn = state.db().lock().await;
            db::get_meeting(&conn, meeting_id).ok().flatten()
        };
        if let Some(meeting) = meeting {
            state
                .publish(meeting_id, json!({"type":"meeting","meeting": meeting}))
                .await;
        }
    }
}

/// 区間の処理中に panic しても、その区間を失敗にして次へ進む。
/// processing のまま残すと、再起動のたびに同じ区間で止まり続けるため。
async fn run_guarded(state: &AppState, job: db::AudioJob) {
    let job_for_panic = job.clone();
    if AssertUnwindSafe(process_audio(state, job))
        .catch_unwind()
        .await
        .is_err()
    {
        fail_job(state, &job_for_panic, "区間の処理が異常終了しました").await;
    }
}

pub fn spawn_prefetch(state: AppState, meeting_id: i64) {
    tokio::spawn(async move {
        if let Err(err) = prefetch(&state, meeting_id).await {
            tracing::warn!("ナレッジの先読みに失敗しました: {err}");
            state
                .publish(
                    meeting_id,
                    json!({"type":"warning","message": err.to_string()}),
                )
                .await;
        }
    });
}

/// 画面の「文字起こし待ち」「再試行待ち」「失敗」の件数を配る。
pub async fn publish_jobs(state: &AppState, meeting_id: i64) {
    let snapshot = {
        let conn = state.db().lock().await;
        let pending = db::pending_jobs(&conn, meeting_id).unwrap_or(0);
        let (waiting, next) = db::retry_waiting(&conn, meeting_id).unwrap_or((0, None));
        let failed = db::count_error_jobs(&conn, meeting_id).unwrap_or(0);
        let errors = db::job_errors(&conn, meeting_id).unwrap_or_default();
        json!({
            "type": "jobs",
            "pending": pending,
            "retry_waiting": waiting,
            "next_retry_at": next,
            "failed": failed,
            "errors": errors
        })
    };
    state.publish(meeting_id, snapshot).await;
}

async fn process_audio(state: &AppState, job: db::AudioJob) {
    tracing::info!(
        meeting_id = job.meeting_id,
        start_ms = job.start_ms,
        "文字起こしを開始します。録音の受付は止めていません"
    );
    let wav_path = db::resolve_audio_path(state.data_dir(), job.meeting_id, &job.wav_path);
    let wav = match tokio::fs::read(&wav_path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            fail_job(state, &job, &format!("音声を読めません: {err}")).await;
            return;
        }
    };
    // キーは会議の持ち主のものを、処理の直前に読む。待ち行列にキーの写しは持たない。
    let (owner, settings, recent, questions, transcript_only) = {
        let conn = state.db().lock().await;
        let owner = match db::meeting_owner(&conn, job.meeting_id) {
            Ok(Some(Some(owner))) => owner,
            Ok(_) => {
                drop(conn);
                fail_job(state, &job, "会議の持ち主が未設定のため、文字起こしできません").await;
                return;
            }
            Err(err) => {
                drop(conn);
                fail_job(state, &job, &err.to_string()).await;
                return;
            }
        };
        let settings = match db::get_user_settings(&conn, owner) {
            Ok(settings) => settings,
            Err(err) => {
                drop(conn);
                fail_job(state, &job, &format!("設定を読めません: {err}")).await;
                return;
            }
        };
        let recent = db::recent_transcript(&conn, job.meeting_id, job.start_ms).unwrap_or_default();
        let questions = db::list_questions(&conn, job.meeting_id).unwrap_or_default();
        let transcript_only = db::meeting_ears(&conn, job.meeting_id).map(|(_, only)| only).unwrap_or(false);
        (owner, settings, recent, questions, transcript_only)
    };
    // 文字だけのときは、回答や要約用のキーが無くても文字起こし用のキーで進める。
    let mut ai = match if transcript_only {
        Ai::for_transcribe(state, owner, &settings).await
    } else {
        Ai::from_settings(state, owner, &settings).await
    } {
        Ok(ai) => ai,
        Err(err) => {
            fail_job(state, &job, &err).await;
            return;
        }
    };
    let mut quick_retry = false;
    let mut switches = 0;
    let analysis = loop {
        let heard = if transcript_only {
            ai.transcribe_wav(state, &wav, &recent).await.map(|text| gemini::ChunkAnalysis {
                transcript: text,
                ..Default::default()
            })
        } else {
            ai.analyze(state, &wav, &recent, &questions).await
        };
        match heard {
            Ok(parsed) => break parsed,
            Err(err) if gemini::should_switch_model(&err) && switches < 2 => {
                tracing::warn!(
                    meeting_id = job.meeting_id,
                    start_ms = job.start_ms,
                    "モデル {} はこの音声に使えないため、別のモデルを試します: {err}",
                    ai.label()
                );
                match ai.switch_model(state, owner, &err).await {
                    Ok(()) => switches += 1,
                    Err(pick) => {
                        fail_job(state, &job, &format!("{err}（{pick}）")).await;
                        return;
                    }
                }
            }
            Err(err) if gemini::is_transient(&err) && !quick_retry => {
                let hinted = gemini::retry_after_secs(&err);
                if hinted.unwrap_or(0) > 15 {
                    defer_job(state, &job, &err).await;
                    return;
                }
                let wait = hinted.unwrap_or(2);
                tracing::warn!(
                    meeting_id = job.meeting_id,
                    start_ms = job.start_ms,
                    "モデル {} が一時的に応答しないため、{wait} 秒後に同じモデルでやり直します",
                    ai.label()
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
                quick_retry = true;
            }
            Err(err) if gemini::is_transient(&err) => {
                defer_job(state, &job, &err).await;
                return;
            }
            Err(err) => {
                fail_job(state, &job, &err).await;
                return;
            }
        }
    };

    let start_sec = job.start_ms as f64 / 1000.0;
    let mut queries = analysis.search_queries.clone();
    let (segment, questions, done_meeting, notes, hint) = {
        let conn = state.db().lock().await;
        let previous_active = db::get_meeting(&conn, job.meeting_id)
            .ok()
            .flatten()
            .and_then(|meeting| meeting.active_question_id);
        let segment: Option<db::Segment> = if analysis.transcript.is_empty()
            || db::segment_exists(&conn, job.meeting_id, start_sec).unwrap_or(false)
        {
            None
        } else {
            match db::insert_segment(&conn, job.meeting_id, start_sec, &analysis.transcript) {
                Ok(id) => Some(db::Segment {
                    id,
                    start_sec,
                    text: analysis.transcript.clone(),
                    ..Default::default()
                }),
                Err(err) => {
                    drop(conn);
                    fail_job(state, &job, &err.to_string()).await;
                    return;
                }
            }
        };
        let questions = db::apply_answers(
            &conn,
            job.meeting_id,
            analysis.active_question_id,
            &analysis.answers,
            segment.as_ref().map(|segment| segment.id),
        )
        .unwrap_or(questions);
        // 聞き手の補助: 聞くことの追加・見送り、メモ、次に聞くとよいこと（発言があった区間だけ）。
        let mut questions = questions;
        let mut notes = None;
        let mut hint = None;
        if let Some(segment) = segment.as_ref() {
            if db::ai_adjust_questions(&conn, job.meeting_id, &analysis.new_questions, &analysis.skip_questions)
                .unwrap_or(false)
            {
                questions = db::list_questions(&conn, job.meeting_id).unwrap_or(questions);
            }
            if db::add_notes(&conn, job.meeting_id, &analysis.notes, Some(segment.id)).unwrap_or(0) > 0 {
                notes = db::list_notes(&conn, job.meeting_id).ok();
            }
            if let Some(next) = analysis.next_question.as_deref() {
                if db::set_next_hint(&conn, job.meeting_id, next).is_ok() {
                    hint = Some(next.to_string());
                }
            }
        }
        // いま話題の聞くことが変わったら、その質問文でも資料を探す。
        if let Some(active) = analysis.active_question_id.filter(|id| Some(*id) != previous_active) {
            if let Some(question) = questions.iter().find(|q| q.id == active) {
                gemini::push_query(&mut queries, &question.body.chars().take(30).collect::<String>());
            }
        }
        if let Err(err) = db::mark_job_done(&conn, job.id) {
            tracing::error!("区間の完了記録に失敗しました: {err:#}");
        }
        let done_meeting = db::meeting_status_of(&conn, job.meeting_id).unwrap_or_default()
            == "done"
            && db::unfinished_jobs(&conn, job.meeting_id).unwrap_or(1) == 0;
        (segment, questions, done_meeting, notes, hint)
    };
    if let Some(notes) = notes {
        state.publish(job.meeting_id, json!({"type":"notes","notes": notes})).await;
    }
    if let Some(hint) = hint {
        state.publish(job.meeting_id, json!({"type":"hint","next_hint": hint})).await;
    }

    if let Some(segment) = segment {
        state
            .publish(job.meeting_id, json!({"type":"segment","segment": segment}))
            .await;
    }
    let active = analysis.active_question_id;
    state
        .publish(
            job.meeting_id,
            json!({"type":"questions","active_question_id": active, "questions": questions}),
        )
        .await;
    publish_jobs(state, job.meeting_id).await;

    // 完了済みの会議で遅れて書き起こせた区間があれば、要約を作り直す。
    if done_meeting {
        spawn_summary(state.clone(), job.meeting_id);
    }
    spawn_knowledge(state.clone(), owner, job.meeting_id, queries);
}

/// 会議の議題と聞くことで資料を探す（顧客資料を添付したときなど）。
pub fn spawn_agenda_search(state: AppState, owner: i64, meeting_id: i64) {
    tokio::spawn(async move {
        let queries = {
            let conn = state.db().lock().await;
            let title = db::meeting_title(&conn, meeting_id).unwrap_or_default();
            let questions = db::list_questions(&conn, meeting_id).unwrap_or_default();
            gemini::queries_from_agenda(&title, &questions)
        };
        spawn_knowledge(state, owner, meeting_id, queries);
    });
}

/// ナレッジ検索は文字起こしの処理枠を使わない。検索するのは会議の持ち主の資料だけ。
fn spawn_knowledge(state: AppState, owner: i64, meeting_id: i64, queries: Vec<String>) {
    if queries.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let (hits, warning) = knowledge::search_for_meeting(&state, owner, meeting_id, &queries).await;
        if !hits.is_empty() {
            let stored = {
                let conn = state.db().lock().await;
                db::store_hits(&conn, meeting_id, &hits).ok();
                db::list_hits(&conn, meeting_id).unwrap_or_default()
            };
            state
                .publish(meeting_id, json!({"type":"knowledge","hits": stored}))
                .await;
        }
        if let Some(message) = warning {
            state
                .publish(meeting_id, json!({"type":"warning","message": message}))
                .await;
        }
    });
}

async fn defer_job(state: &AppState, job: &db::AudioJob, message: &str) {
    let attempts = {
        let conn = state.db().lock().await;
        db::job_attempts(&conn, job.id).unwrap_or(0)
    };
    if attempts + 1 >= db::MAX_ATTEMPTS {
        fail_job(state, job, message).await;
        return;
    }
    let wait = gemini::retry_after_secs(message).unwrap_or_else(|| {
        let shift = u32::try_from(attempts).unwrap_or(0).min(6);
        30 * (1u64 << shift)
    });
    let when = db::stamp_after(wait);
    {
        let conn = state.db().lock().await;
        if let Err(err) = db::schedule_retry(&conn, job.id, message, &when) {
            tracing::error!("再試行の予約に失敗しました: {err:#}");
        }
    }
    tracing::warn!(
        meeting_id = job.meeting_id,
        start_ms = job.start_ms,
        "一時的な失敗のため {wait} 秒後に同じモデルで再試行します: {message}"
    );
    publish_jobs(state, job.meeting_id).await;
}

async fn fail_job(state: &AppState, job: &db::AudioJob, message: &str) {
    tracing::warn!(
        meeting_id = job.meeting_id,
        start_ms = job.start_ms,
        "区間の解析に失敗しました: {message}"
    );
    {
        let conn = state.db().lock().await;
        let _ = db::mark_job_error(&conn, job.id, message);
    }
    state
        .publish(
            job.meeting_id,
            json!({"type":"warning","message": format!("文字起こしに失敗した区間があります: {message}")}),
        )
        .await;
    publish_jobs(state, job.meeting_id).await;
}

async fn prefetch(state: &AppState, meeting_id: i64) -> Result<(), String> {
    let (owner, title, questions, chunks, path, fess) = {
        let conn = state.db().lock().await;
        let Some(Some(owner)) = db::meeting_owner(&conn, meeting_id).map_err(|err| err.to_string())? else {
            return Ok(());
        };
        let title = db::meeting_title(&conn, meeting_id).map_err(|err| err.to_string())?;
        let questions = db::list_questions(&conn, meeting_id).map_err(|err| err.to_string())?;
        let stats = db::index_stats(&conn, owner).map_err(|err| err.to_string())?;
        let settings = db::get_user_settings(&conn, owner).map_err(|err| err.to_string())?;
        (
            owner,
            title,
            questions,
            stats.chunks,
            settings.knowledge_path,
            settings.fess_enabled && !settings.fess_url.is_empty(),
        )
    };
    let customer_docs = {
        let conn = state.db().lock().await;
        db::list_meeting_docs(&conn, meeting_id).map(|docs| docs.len()).unwrap_or(0)
    };
    if chunks == 0 && !fess && customer_docs == 0 {
        if path.is_empty() {
            return Ok(());
        }
        return Err("ナレッジが未索引です。設定から再索引してください".into());
    }
    let queries = gemini::queries_from_agenda(&title, &questions);
    let (hits, warning) = knowledge::search_for_meeting(state, owner, meeting_id, &queries).await;
    if !hits.is_empty() {
        let stored = {
            let conn = state.db().lock().await;
            db::store_hits(&conn, meeting_id, &hits).map_err(|err| err.to_string())?;
            db::list_hits(&conn, meeting_id).map_err(|err| err.to_string())?
        };
        state
            .publish(meeting_id, json!({"type":"knowledge","hits": stored}))
            .await;
    }
    if let Some(message) = warning {
        state
            .publish(meeting_id, json!({"type":"warning","message": message}))
            .await;
    }
    Ok(())
}

/// 要約は文字起こしの処理枠と別に走らせる。同じ会議の要約は同時に一つだけ。
pub fn spawn_summary(state: AppState, meeting_id: i64) -> bool {
    if !state.begin_summary(meeting_id) {
        return false;
    }
    tokio::spawn(async move {
        let result = AssertUnwindSafe(summarize(&state, meeting_id))
            .catch_unwind()
            .await;
        state.end_summary(meeting_id);
        if result.is_err() {
            tracing::error!(meeting_id, "要約の処理が異常終了しました");
            let conn = state.db().lock().await;
            let _ = db::save_summary(&conn, meeting_id, "", Some("要約の処理が異常終了しました"));
        }
        state.notify_jobs();
    });
    true
}

async fn summarize(state: &AppState, meeting_id: i64) {
    let unfinished = {
        let conn = state.db().lock().await;
        db::unfinished_jobs(&conn, meeting_id).unwrap_or(1)
    };
    if unfinished > 0 {
        let conn = state.db().lock().await;
        let _ = db::release_finish_claim(&conn, meeting_id);
        return;
    }
    state
        .publish(meeting_id, json!({"type":"summarizing","running": true}))
        .await;

    // API の文字起こしが打ち切りになった区間は、速報字幕の控えで埋めてから材料を読む。
    let patched = {
        let conn = state.db().lock().await;
        match db::fill_gaps_from_backup(&conn, meeting_id) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(meeting_id, "速報字幕の控えで欠落を埋められません: {err:#}");
                0
            }
        }
    };
    if patched > 0 {
        tracing::info!(meeting_id, patched, "文字起こしが欠けた区間を、速報字幕の控えで埋めました");
    }
    let loaded = {
        let conn = state.db().lock().await;
        let owner = db::meeting_owner(&conn, meeting_id).ok().flatten().flatten();
        let settings = match owner {
            Some(owner) => db::get_user_settings(&conn, owner),
            None => Err(anyhow::anyhow!("会議の持ち主が未設定です")),
        };
        (
            export::load_minutes(&conn, meeting_id),
            settings.map(|settings| (owner.unwrap_or(0), settings)),
            db::count_error_jobs(&conn, meeting_id).unwrap_or(0),
            db::finish_close(&conn, meeting_id).unwrap_or_default(),
        )
    };
    let (mut minutes, (owner, settings), missing, finish_close) = match loaded {
        (Ok(Some(minutes)), Ok(settings), missing, close) => (minutes, settings, missing, close),
        (Ok(None), _, _, _) => return,
        (Err(err), _, _, _) | (_, Err(err), _, _) => {
            // 材料が読めないまま「まとめ中」に残すと、選び直しが止まらない。失敗として完了させる。
            tracing::error!("要約の材料を読めません: {err:#}");
            let message = format!("要約の材料を読めませんでした: {err}");
            {
                let conn = state.db().lock().await;
                let _ = db::save_summary(&conn, meeting_id, "", Some(&message));
            }
            publish_done(state, meeting_id).await;
            return;
        }
    };

    if finish_close == "plain" {
        let summary = note_gaps(export::plain_minutes(&minutes), missing, patched);
        {
            let conn = state.db().lock().await;
            if let Err(err) = db::save_summary(&conn, meeting_id, &summary, None) {
                tracing::error!("議事録の保存に失敗しました: {err:#}");
                return;
            }
        }
        publish_done(state, meeting_id).await;
        return;
    }

    fill_participants(state, meeting_id, owner, &settings, &mut minutes).await;
    infer_speakers(state, meeting_id, owner, &settings, &mut minutes).await;

    let no_material = minutes.segments.is_empty()
        && minutes.pins.is_empty()
        && minutes
            .questions
            .iter()
            .all(|question| question.answer.trim().is_empty());
    let summary_result = if no_material {
        Err("書き起こしがありません".to_string())
    } else {
        // キーの有無は、選んでいる提供元（Gemini / OpenAI）ごとに Ai::from_settings で確かめる。
        request_summary(state, owner, &settings, &summary_context(&minutes)).await
    };

    let unfinished_after = {
        let conn = state.db().lock().await;
        db::unfinished_jobs(&conn, meeting_id).unwrap_or(0)
    };
    if unfinished_after > 0 {
        let conn = state.db().lock().await;
        let _ = db::release_finish_claim(&conn, meeting_id);
        state.notify_jobs();
        return;
    }

    let (summary, error) = match summary_result {
        Ok(summary) => (note_gaps(summary, missing, patched), None),
        Err(err) => (
            note_gaps(export::fallback_summary(&minutes, &err), missing, patched),
            Some(err),
        ),
    };
    {
        let conn = state.db().lock().await;
        if let Err(err) = db::save_summary(&conn, meeting_id, &summary, error.as_deref()) {
            tracing::error!("要約の保存に失敗しました: {err:#}");
            return;
        }
    }
    publish_done(state, meeting_id).await;
}

/// 要約を作るとき（作り直すときも）、参加者を書き起こしから拾って入れる。
/// 対象は、参加者が空欄の会議と、前回自動で入れた会議だけ。人が入れた参加者は上書きしない。
/// 拾えなくても要約は続ける（参加者は要約の材料の一部にすぎない）。
/// 会議の終了後に、発言ごとの話者を推定して入れる（#83）。人が付けた話者は変えない。
/// 書き起こしを 150 発言ずつ渡し、前の区切りで使った呼び名を次に引き継いで、同じ人に同じ呼び名を付けさせる。
async fn infer_speakers(state: &AppState, meeting_id: i64, owner: i64, settings: &db::Settings, minutes: &mut Minutes) {
    if minutes.segments.len() < 2 {
        return;
    }
    let Ok(ai) = Ai::from_settings(state, owner, settings).await else {
        return;
    };
    let mut found: Vec<(i64, String)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    // 長い会議でも呼び出しが増えすぎないよう、2000 発言までにする。
    for batch in minutes.segments.iter().take(2000).collect::<Vec<_>>().chunks(150) {
        let mut lines = String::new();
        if !minutes.participants.trim().is_empty() {
            lines.push_str(&format!("参加者: {}\n", minutes.participants.trim()));
        }
        if !labels.is_empty() {
            lines.push_str(&format!("これまでの呼び名: {}\n", labels.join("、")));
        }
        lines.push_str("\n書き起こし:\n");
        for segment in batch {
            let whole = segment.start_sec.floor() as i64;
            lines.push_str(&format!("#{} [{:02}:{:02}] {}\n", segment.id, whole / 60, whole % 60, segment.text));
        }
        match ai.speakers(state, &lines).await {
            Ok(items) => {
                let ids: Vec<i64> = batch.iter().map(|s| s.id).collect();
                for (id, speaker) in items.into_iter().filter(|(id, _)| ids.contains(id)) {
                    if !labels.contains(&speaker) {
                        labels.push(speaker.clone());
                    }
                    found.push((id, speaker));
                }
            }
            Err(err) => {
                tracing::warn!(meeting_id, "話者を推定できませんでした: {err}");
                return;
            }
        }
    }
    if found.is_empty() {
        return;
    }
    let segments = {
        let conn = state.db().lock().await;
        if db::set_auto_speakers(&conn, meeting_id, &found).is_err() {
            return;
        }
        db::list_segments(&conn, meeting_id).unwrap_or_default()
    };
    minutes.segments = segments.clone();
    for segment in segments.iter().filter(|s| !s.speaker.is_empty()) {
        state.publish(meeting_id, json!({"type":"segment","segment": segment, "edited": true})).await;
    }
}

async fn fill_participants(
    state: &AppState,
    meeting_id: i64,
    owner: i64,
    settings: &db::Settings,
    minutes: &mut Minutes,
) {
    let manual = !minutes.participants.trim().is_empty() && !minutes.participants_auto;
    if manual || minutes.segments.is_empty() {
        return;
    }
    let Ok(ai) = Ai::from_settings(state, owner, settings).await else {
        return;
    };
    // 名乗りは冒頭に多いので、冒頭を厚めに、終盤も少し渡す。
    let lines: Vec<String> = minutes.segments.iter().map(|segment| segment.text.clone()).collect();
    let joined = lines.join("\n");
    let transcript = if joined.chars().count() <= 30_000 {
        joined
    } else {
        let head: String = joined.chars().take(20_000).collect();
        let tail: String = joined.chars().rev().take(10_000).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}\n…\n{tail}")
    };
    let names = match ai.participants(state, &transcript).await {
        Ok(names) if !names.is_empty() => names,
        Ok(_) => return,
        Err(err) => {
            tracing::warn!(meeting_id, "参加者を拾えませんでした: {err}");
            return;
        }
    };
    let text = names.join("、");
    if text == minutes.participants {
        return;
    }
    let updated = {
        let conn = state.db().lock().await;
        // 拾っている間に人が入れていたら、そちらを優先する（SQL の条件で確かめる）。
        db::set_auto_participants(&conn, meeting_id, &text).unwrap_or(false)
    };
    if !updated {
        return;
    }
    minutes.participants = text.clone();
    minutes.participants_auto = true;
    state
        .publish(
            meeting_id,
            json!({"type":"warning","message": format!("発言から参加者を拾って入れました: {text}（見出しの「✎ 編集」で直せます）")}),
        )
        .await;
}

/// 取り込んだ書き起こし（#96）を 1 分ほど・1500 文字ほどずつ AI に渡し、回答候補・メモ・資料を作る。
/// 完了済みの会議なら、最後に要約を作り直す。
pub fn spawn_text_import(state: AppState, meeting_id: i64, segments: Vec<db::Segment>) {
    tokio::spawn(async move {
        let mut windows: Vec<Vec<db::Segment>> = Vec::new();
        for segment in segments {
            let start_new = match windows.last() {
                Some(window) => {
                    let first = window[0].start_sec;
                    let chars: usize = window.iter().map(|s| s.text.chars().count()).sum();
                    segment.start_sec - first > 60.0 || chars > 1500
                }
                None => true,
            };
            if start_new {
                windows.push(vec![segment]);
            } else if let Some(window) = windows.last_mut() {
                window.push(segment);
            }
        }
        let total = windows.len();
        for (index, window) in windows.into_iter().enumerate() {
            if let Err(err) = analyze_imported(&state, meeting_id, &window).await {
                state
                    .publish(meeting_id, json!({"type":"warning","message": format!("取り込んだ書き起こしの解析に失敗しました（{}/{total}）: {err}", index + 1)}))
                    .await;
                if !gemini::is_transient(&err) {
                    break;
                }
            }
        }
        let done = {
            let conn = state.db().lock().await;
            db::meeting_status_of(&conn, meeting_id).unwrap_or_default() == "done"
                && db::unfinished_jobs(&conn, meeting_id).unwrap_or(1) == 0
        };
        if done {
            spawn_summary(state.clone(), meeting_id);
        }
    });
}

async fn analyze_imported(state: &AppState, meeting_id: i64, window: &[db::Segment]) -> Result<(), String> {
    let first = &window[0];
    let text: String = window.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n");
    let (owner, settings, recent, questions) = {
        let conn = state.db().lock().await;
        let owner = db::meeting_owner(&conn, meeting_id)
            .map_err(|err| err.to_string())?
            .flatten()
            .ok_or("会議がありません")?;
        let settings = db::get_user_settings(&conn, owner).map_err(|err| err.to_string())?;
        let recent = db::recent_transcript(&conn, meeting_id, (first.start_sec * 1000.0) as i64).unwrap_or_default();
        let questions = db::list_questions(&conn, meeting_id).map_err(|err| err.to_string())?;
        (owner, settings, recent, questions)
    };
    let ai = Ai::from_settings(state, owner, &settings).await?;
    let mut attempt = 0;
    let analysis = loop {
        match ai.analyze_text(state, &text, &recent, &questions).await {
            Ok(analysis) => break analysis,
            Err(err) if gemini::is_transient(&err) && attempt < 2 => {
                attempt += 1;
                let wait = gemini::retry_after_secs(&err).unwrap_or(5).min(60);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            Err(err) => return Err(err),
        }
    };
    let mut queries = analysis.search_queries.clone();
    let (questions, notes) = {
        let conn = state.db().lock().await;
        let mut questions = db::apply_answers(&conn, meeting_id, analysis.active_question_id, &analysis.answers, Some(first.id))
            .map_err(|err| err.to_string())?;
        if db::ai_adjust_questions(&conn, meeting_id, &analysis.new_questions, &analysis.skip_questions).unwrap_or(false) {
            questions = db::list_questions(&conn, meeting_id).unwrap_or(questions);
        }
        let notes = (db::add_notes(&conn, meeting_id, &analysis.notes, Some(first.id)).unwrap_or(0) > 0)
            .then(|| db::list_notes(&conn, meeting_id).ok())
            .flatten();
        if let Some(active) = analysis.active_question_id {
            if let Some(question) = questions.iter().find(|q| q.id == active) {
                gemini::push_query(&mut queries, &question.body.chars().take(30).collect::<String>());
            }
        }
        (questions, notes)
    };
    state
        .publish(meeting_id, json!({"type":"questions","active_question_id": analysis.active_question_id, "questions": questions}))
        .await;
    if let Some(notes) = notes {
        state.publish(meeting_id, json!({"type":"notes","notes": notes})).await;
    }
    spawn_knowledge(state.clone(), owner, meeting_id, queries);
    Ok(())
}

const RECAP_PROMPT: &str = "あなたは顧客ヒアリングの聞き手を支える係です。次の書き起こし（会議の直近の部分）から、\
話の流れを3行の箇条書きでまとめてください。1行は40文字以内です。\
最後に「まだ聞けていないこと」を1行だけ添えてください（無ければ省きます）。\
書き起こしに無いことは書かないでください。出力は Markdown の箇条書きだけです。";

/// 「ここまでの要点」。直近 10 分ほどの書き起こしを 3 行にまとめる（会議の持ち主のキーで）。
pub async fn recap(state: &AppState, meeting_id: i64) -> Result<String, String> {
    let (owner, settings, context) = {
        let conn = state.db().lock().await;
        let owner = db::meeting_owner(&conn, meeting_id)
            .map_err(|err| err.to_string())?
            .flatten()
            .ok_or("会議がありません")?;
        let settings = db::get_user_settings(&conn, owner).map_err(|err| err.to_string())?;
        let segments = db::list_segments(&conn, meeting_id).map_err(|err| err.to_string())?;
        let questions = db::list_questions(&conn, meeting_id).map_err(|err| err.to_string())?;
        let last = segments.last().map(|s| s.start_sec).unwrap_or(0.0);
        let mut context = String::from("# アジェンダ（状態）\n");
        for question in &questions {
            context.push_str(&format!("- {}（{}）\n", question.body, export::question_label(question)));
        }
        context.push_str("\n# 直近の書き起こし\n");
        for segment in segments.iter().filter(|s| s.start_sec >= last - 600.0) {
            let whole = segment.start_sec.floor() as i64;
            context.push_str(&format!("[{:02}:{:02}] {}\n", whole / 60, whole % 60, segment.text));
        }
        if segments.is_empty() {
            return Err("まだ書き起こしがありません".into());
        }
        (owner, settings, context)
    };
    let ai = Ai::from_settings(state, owner, &settings).await?;
    ai.summarize(state, RECAP_PROMPT, &context).await
}

/// 一時的な失敗は少し待って同じモデルでやり直す。モデルが使えないときだけ切り替える。
async fn request_summary(
    state: &AppState,
    owner: i64,
    settings: &db::Settings,
    context: &str,
) -> Result<String, String> {
    let mut ai = Ai::from_settings(state, owner, settings).await?;
    let mut switched = false;
    let mut attempt = 0;
    loop {
        match ai.summarize(state, &settings.summary_prompt, context).await {
            Ok(summary) => return Ok(summary),
            Err(err) if gemini::should_switch_model(&err) && !switched => {
                switched = true;
                ai.switch_model(state, owner, &err)
                    .await
                    .map_err(|pick| format!("{err}（{pick}）"))?;
            }
            Err(err) if gemini::is_transient(&err) && attempt < 2 => {
                let wait = gemini::retry_after_secs(&err).unwrap_or(5 * (attempt + 1) * (attempt + 1));
                if wait > 60 {
                    return Err(err);
                }
                attempt += 1;
                tracing::warn!("要約が一時的に失敗したため、{wait} 秒後にやり直します: {err}");
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// 会員が選んだ AI の提供元と、そのキー・モデル。処理の直前に設定から作る。
enum Ai {
    Gemini {
        key: String,
        model: String,
    },
    OpenAi {
        key: String,
        transcribe: String,
        chat: String,
    },
    /// 回答・要約は Claude、文字起こしは `ears`（Gemini か OpenAI）。
    /// 文字起こしのキーが無くても要約はできるよう、作れなかった理由をそのまま持つ。
    Claude {
        key: String,
        model: String,
        ears: Result<Box<Ai>, String>,
    },
}

impl Ai {
    async fn from_settings(state: &AppState, owner: i64, settings: &db::Settings) -> Result<Ai, String> {
        if settings.uses_anthropic() {
            if settings.anthropic_api_key.is_empty() {
                return Err("設定画面で Claude の APIキーを登録してください".into());
            }
            let mut model = settings.anthropic_model.clone();
            if model.is_empty() {
                model = save_anthropic_model(state, owner, &settings.anthropic_api_key, "").await?;
            }
            let ears = Ai::for_transcribe(state, owner, settings)
                .await
                .map(Box::new)
                .map_err(|err| format!("{err}（Claude は音声を受け付けないため、文字起こしに使います）"));
            return Ok(Ai::Claude {
                key: settings.anthropic_api_key.clone(),
                model,
                ears,
            });
        }
        Ai::for_transcribe(state, owner, settings).await
    }

    /// 文字起こしもできる提供元（Gemini / OpenAI）。
    async fn for_transcribe(state: &AppState, owner: i64, settings: &db::Settings) -> Result<Ai, String> {
        if settings.transcribe_provider() == "openai" {
            if settings.openai_api_key.is_empty() {
                return Err("設定画面で OpenAI の APIキーを登録してください".into());
            }
            let (mut transcribe, mut chat) = (
                settings.openai_transcribe_model.clone(),
                settings.openai_model.clone(),
            );
            if transcribe.is_empty() || chat.is_empty() {
                (transcribe, chat) = save_openai_models(state, owner, &settings.openai_api_key, "").await?;
            }
            return Ok(Ai::OpenAi {
                key: settings.openai_api_key.clone(),
                transcribe,
                chat,
            });
        }
        if settings.api_key.is_empty() {
            return Err("設定画面で Gemini の APIキーを登録してください".into());
        }
        Ok(Ai::Gemini {
            key: settings.api_key.clone(),
            model: settings.model.clone(),
        })
    }

    fn label(&self) -> String {
        match self {
            Ai::Gemini { model, .. } => model.clone(),
            Ai::OpenAi { transcribe, chat, .. } => format!("{transcribe} / {chat}"),
            Ai::Claude { model, ears, .. } => match ears.as_deref() {
                Ok(Ai::Gemini { model: heard, .. }) => format!("{heard} / {model}"),
                Ok(Ai::OpenAi { transcribe, .. }) => format!("{transcribe} / {model}"),
                _ => model.clone(),
            },
        }
    }

    /// 音声を文字にするだけ。回答やナレッジは付けない。
    async fn transcribe_wav(&self, state: &AppState, wav: &[u8], recent: &str) -> Result<String, String> {
        match self {
            Ai::Gemini { key, model } => gemini::transcribe(state.http(), key, model, wav, recent).await,
            Ai::OpenAi { key, transcribe, .. } => openai::transcribe(state.http(), key, transcribe, wav, recent).await,
            Ai::Claude { ears, .. } => match ears.as_deref() {
                Ok(Ai::Gemini { key, model }) => gemini::transcribe(state.http(), key, model, wav, recent).await,
                Ok(Ai::OpenAi { key, transcribe, .. }) => openai::transcribe(state.http(), key, transcribe, wav, recent).await,
                Ok(Ai::Claude { .. }) => Err("文字起こしの提供元が正しくありません".into()),
                Err(err) => Err(err.clone()),
            },
        }
    }

    async fn analyze(
        &self,
        state: &AppState,
        wav: &[u8],
        recent: &str,
        questions: &[db::Question],
    ) -> Result<gemini::ChunkAnalysis, String> {
        match self {
            Ai::Gemini { key, model } => {
                gemini::analyze_chunk(state.http(), key, model, wav, recent, questions).await
            }
            Ai::OpenAi { key, transcribe, chat } => {
                openai::analyze_chunk(state.http(), key, transcribe, chat, wav, recent, questions).await
            }
            Ai::Claude { key, model, ears } => {
                let transcript = match ears.as_deref() {
                    Ok(Ai::Gemini { key, model }) => gemini::transcribe(state.http(), key, model, wav, recent).await?,
                    Ok(Ai::OpenAi { key, transcribe, .. }) => {
                        openai::transcribe(state.http(), key, transcribe, wav, recent).await?
                    }
                    Ok(Ai::Claude { .. }) => return Err("文字起こしの提供元が正しくありません".into()),
                    Err(err) => return Err(err.clone()),
                };
                anthropic::analyze_text(state.http(), key, model, transcript, recent, questions).await
            }
        }
    }

    /// 取り込んだ書き起こし（文字）の解析。音声は使わない。
    async fn analyze_text(
        &self,
        state: &AppState,
        transcript: &str,
        recent: &str,
        questions: &[db::Question],
    ) -> Result<gemini::ChunkAnalysis, String> {
        let text = transcript.to_string();
        match self {
            Ai::Gemini { key, model } => gemini::analyze_text(state.http(), key, model, text, recent, questions).await,
            Ai::OpenAi { key, chat, .. } => openai::analyze_text(state.http(), key, chat, text, recent, questions).await,
            Ai::Claude { key, model, .. } => anthropic::analyze_text(state.http(), key, model, text, recent, questions).await,
        }
    }

    async fn participants(&self, state: &AppState, transcript: &str) -> Result<Vec<String>, String> {
        match self {
            Ai::Gemini { key, model } => gemini::participants(state.http(), key, model, transcript).await,
            Ai::OpenAi { key, chat, .. } => openai::participants(state.http(), key, chat, transcript).await,
            Ai::Claude { key, model, .. } => anthropic::participants(state.http(), key, model, transcript).await,
        }
    }

    async fn speakers(&self, state: &AppState, lines: &str) -> Result<Vec<(i64, String)>, String> {
        match self {
            Ai::Gemini { key, model } => gemini::speakers(state.http(), key, model, lines).await,
            Ai::OpenAi { key, chat, .. } => openai::speakers(state.http(), key, chat, lines).await,
            Ai::Claude { key, model, .. } => anthropic::speakers(state.http(), key, model, lines).await,
        }
    }

    async fn summarize(&self, state: &AppState, prompt: &str, context: &str) -> Result<String, String> {
        match self {
            Ai::Gemini { key, model } => gemini::summarize(state.http(), key, model, prompt, context).await,
            Ai::OpenAi { key, chat, .. } => openai::summarize(state.http(), key, chat, prompt, context).await,
            Ai::Claude { key, model, .. } => anthropic::summarize(state.http(), key, model, prompt, context).await,
        }
    }

    /// 使えなかったモデルを避けて選び直し、その会員の設定に保存する。
    /// Claude のときは、失敗した側（Claude か文字起こし）だけを選び直す。
    async fn switch_model(&mut self, state: &AppState, owner: i64, failure: &str) -> Result<(), String> {
        match self {
            Ai::Claude { key, model, ears } => {
                if failure.contains(" Claude:") {
                    *model = save_anthropic_model(state, owner, key, model).await?;
                    return Ok(());
                }
                match ears {
                    Ok(ears) => switch_transcriber(ears, state, owner).await,
                    Err(err) => Err(err.clone()),
                }
            }
            other => switch_transcriber(other, state, owner).await,
        }
    }
}

async fn switch_transcriber(ai: &mut Ai, state: &AppState, owner: i64) -> Result<(), String> {
    match ai {
        Ai::Gemini { key, model } => {
            *model = replace_model(state, owner, key, model).await?;
        }
        Ai::OpenAi { key, transcribe, chat } => {
            let skip = chat.clone();
            let (next_transcribe, next_chat) = save_openai_models(state, owner, key, &skip).await?;
            *transcribe = next_transcribe;
            *chat = next_chat;
        }
        Ai::Claude { .. } => return Err("文字起こしの提供元が正しくありません".into()),
    }
    Ok(())
}

async fn save_anthropic_model(state: &AppState, owner: i64, key: &str, skip: &str) -> Result<String, String> {
    let model = anthropic::choose_model(state.http(), key, skip).await?;
    let conn = state.db().lock().await;
    db::patch_user_settings(
        &conn,
        owner,
        db::SettingsPatch {
            anthropic_model: Some(model.clone()),
            ..Default::default()
        },
    )
    .map_err(|err| format!("使用モデルを保存できません: {err:#}"))?;
    Ok(model)
}

async fn save_openai_models(
    state: &AppState,
    owner: i64,
    key: &str,
    skip: &str,
) -> Result<(String, String), String> {
    let (transcribe, chat) = openai::choose_models(state.http(), key, skip).await?;
    let conn = state.db().lock().await;
    db::patch_user_settings(
        &conn,
        owner,
        db::SettingsPatch {
            openai_model: Some(chat.clone()),
            openai_transcribe_model: Some(transcribe.clone()),
            ..Default::default()
        },
    )
    .map_err(|err| format!("使用モデルを保存できません: {err:#}"))?;
    Ok((transcribe, chat))
}

async fn publish_done(state: &AppState, meeting_id: i64) {
    let meeting = {
        let conn = state.db().lock().await;
        db::get_meeting(&conn, meeting_id).ok().flatten()
    };
    let Some(meeting) = meeting else {
        return;
    };
    state
        .publish(
            meeting_id,
            json!({
                "type": "status",
                "status": "done",
                "summary_md": meeting.summary_md,
                "error": meeting.error,
                "meeting": meeting
            }),
        )
        .await;
    state
        .publish(meeting_id, json!({"type":"summarizing","running": false}))
        .await;
}

fn note_gaps(summary: String, missing: i64, patched: i64) -> String {
    if missing <= 0 {
        return summary;
    }
    if patched > 0 {
        return format!(
            "> 書き起こし欠落: {missing} 区間（うち {patched} 区間は速報字幕で補いました。録音は残っています）\n\n{summary}"
        );
    }
    format!("> 書き起こし欠落: {missing} 区間（録音は残っています）\n\n{summary}")
}

fn summary_context(minutes: &Minutes) -> String {
    let mut head = String::new();
    if !minutes.participants.trim().is_empty() {
        head.push_str(&format!("# 参加者\n{}\n\n", minutes.participants.trim()));
    }
    head.push_str("# アジェンダ（議題 / 状態・目安と経過 / 回答・結論）\n");
    for question in &minutes.questions {
        head.push_str(&format!(
            "- {} / {}{} / {}\n",
            question.body,
            export::question_label(question),
            export::agenda_time(question, minutes.duration_sec),
            question.answer
        ));
    }
    let unanswered: Vec<&str> = minutes
        .questions
        .iter()
        .filter(|q| q.status == "open" || (q.status == "draft" && q.answer.trim().is_empty()))
        .map(|q| q.body.as_str())
        .collect();
    if !unanswered.is_empty() {
        head.push_str("\n# 聞けなかったこと（次回確認すること の材料）\n");
        for body in unanswered {
            head.push_str(&format!("- {body}\n"));
        }
    }
    if !minutes.notes.is_empty() {
        head.push_str("\n# 会議中に AI が発言から拾ったメモ（決定・宿題・数字・懸念・用語）\n");
        for note in &minutes.notes {
            let detail = if note.detail.is_empty() { String::new() } else { format!("（{}）", note.detail) };
            head.push_str(&format!("- {}: {}{detail}\n", export::note_label(&note.kind), note.text));
        }
    }
    let marked: Vec<&db::Segment> = minutes.segments.iter().filter(|s| minutes.marked(s)).collect();
    if !marked.is_empty() {
        head.push_str("\n# 聞き手が「ここ重要」と印を付けた発言（要約で重視すること）\n");
        for segment in marked {
            let whole = segment.start_sec.floor() as i64;
            head.push_str(&format!("- [{:02}:{:02}] {}\n", whole / 60, whole % 60, segment.text));
        }
    }
    head.push_str("\n# ピン留めした資料（社内ナレッジ・顧客から受け取った資料）\n");
    for pin in &minutes.pins {
        let source = if pin.source == "customer" { "顧客資料" } else { "社内" };
        head.push_str(&format!(
            "- [{source}] {} ({}) 検索語:{} {}\n",
            pin.title, pin.locator, pin.query, pin.snippet
        ));
    }
    const LIMIT: usize = 100_000;
    let head_len = head.chars().count();
    if head_len >= LIMIT {
        let trimmed: String = head.chars().take(LIMIT.saturating_sub(48)).collect();
        return format!("> 材料が長いため、質問とナレッジを優先して収めています。\n\n{trimmed}");
    }
    let note = "> 書き起こしは長いため、会議の終盤を優先して収めています。\n\n";
    let header = "\n# 書き起こし\n";
    let (body, truncated) = transcript_tail(
        minutes,
        LIMIT
            .saturating_sub(head_len)
            .saturating_sub(header.chars().count()),
    );
    if !truncated {
        return format!("{head}{header}{body}");
    }
    let budget = LIMIT
        .saturating_sub(head_len)
        .saturating_sub(note.chars().count())
        .saturating_sub(header.chars().count());
    let (body, _) = transcript_tail(minutes, budget);
    format!("{note}{head}{header}{body}")
}

fn transcript_tail(minutes: &Minutes, budget: usize) -> (String, bool) {
    let mut chosen = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for segment in minutes.segments.iter().rev() {
        let whole = segment.start_sec.floor() as i64;
        let who = if segment.speaker.is_empty() { String::new() } else { format!("{}: ", segment.speaker) };
        let line = format!(
            "[{:02}:{:02}] {who}{}\n",
            whole / 60,
            whole % 60,
            segment.text
        );
        let len = line.chars().count();
        if used + len > budget {
            truncated = true;
            break;
        }
        used += len;
        chosen.push(line);
    }
    chosen.reverse();
    (chosen.concat(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Hit, Question, Segment};
    use serde_json::Value;

    #[test]
    fn summary_keeps_questions_and_the_end_of_a_long_transcript() {
        let segments = (0..5000)
            .map(|index| Segment {
                id: index,
                start_sec: index as f64,
                text: format!("区間{index:04}の発言です"),
                ..Default::default()
            })
            .collect();
        let minutes = Minutes {
            title: "長い会議".into(),
            summary_md: String::new(),
            questions: vec![Question {
                id: 1,
                position: 0,
                body: "予算は？".into(),
                status: "draft".into(),
                answer: "増額".into(),
                locked: false,
                source: None,
                evidence: Vec::new(),
                origin: "human".into(),
                skipped_by: String::new(),
                skip_reason: String::new(),
                minutes: None,
                spent_sec: 0.0,
                active_since: None,
            }],
            segments,
            pins: vec![Hit {
                id: 1,
                source: "local".into(),
                title: "方針".into(),
                locator: "a.md".into(),
                snippet: "上限".into(),
                query: "予算".into(),
                pinned: true,
            }],
            ..Minutes::default()
        };
        let text = summary_context(&minutes);
        let questions_at = text.find("予算は？").unwrap();
        let transcript_at = text.find("# 書き起こし").unwrap();
        assert!(questions_at < transcript_at);
        assert!(text.contains("方針"));
        assert!(text.contains("区間4999"));
        assert!(!text.contains("区間0000"));
        assert!(text.contains("終盤"));
        assert!(text.chars().count() <= 100_000);
    }

    /// 模擬の Gemini / OpenAI。届いた要求の宛先とキーを記録する。
    async fn mock_ai() -> (String, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        use axum::http::HeaderMap;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let log = seen.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap, body: axum::body::Bytes| {
            let log = log.clone();
            async move {
                // 音声を含む要求もあるので、本文はバイト列で受けて文字列として見る。
                let text = String::from_utf8_lossy(&body).to_string();
                let wants_names = text.contains("参加者の名前を拾って");
                let path = uri.path().to_string();
                if path.starts_with("/anthropic/") {
                    // Claude: キー・API の版・送った本文の要点を記録する。
                    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    let sent: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let note = format!(
                        "{}|{}|{}|fallbacks={}|format={}",
                        header("x-api-key"),
                        header("anthropic-version"),
                        header("anthropic-beta"),
                        sent["fallbacks"].as_str().unwrap_or(""),
                        sent["output_config"]["format"]["type"].as_str().unwrap_or("")
                    );
                    log.lock().unwrap().push((path.clone(), note));
                    let reply = if wants_names {
                        "{\"participants\":[\"高橋さん\"]}".to_string()
                    } else if text.contains("<segment>") {
                        "{\"active_question_id\":null,\"answers\":[],\"search_queries\":[\"予算の確認\"]}".to_string()
                    } else {
                        "## 要約\nClaude の要約".to_string()
                    };
                    return axum::Json(json!({
                        "type": "message",
                        "stop_reason": "end_turn",
                        "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": reply}]
                    }));
                }
                let key = headers
                    .get("x-goog-api-key")
                    .or_else(|| headers.get("authorization"))
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                log.lock().unwrap().push((path.clone(), key));
                let body = if wants_names && path.contains("generateContent") {
                    json!({"candidates": [{"content": {"parts": [{"text": "{\"participants\":[\"田中さん\",\"佐藤\"]}"}]}}]})
                } else if wants_names {
                    json!({"choices": [{"message": {"content": "{\"participants\":[\"鈴木さん\"]}"}}]})
                } else if path.contains("generateContent") {
                    json!({"candidates": [{"content": {"parts": [{"text": "{\"transcript\":\"Gemini の書き起こし\",\"answers\":[],\"search_queries\":[]}"}]}}]})
                } else if path.ends_with("/audio/transcriptions") {
                    json!({"text": "OpenAI の書き起こし"})
                } else {
                    json!({"choices": [{"message": {"content": "{\"answers\":[],\"search_queries\":[]}"}}]})
                };
                axum::Json(body)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    async fn queue_chunk(state: &AppState, user: i64, title: &str) -> (i64, db::AudioJob) {
        let mut conn = state.db().lock().await;
        let meeting = db::create_meeting(&conn, user, title, &[]).unwrap();
        db::set_live(&conn, meeting).unwrap();
        let dir = state.data_dir().join("audio").join(meeting.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("0.wav");
        std::fs::write(&path, crate::audio::encode_wav(&[0, 0, 1, 0], 16_000)).unwrap();
        db::enqueue_audio(&mut conn, meeting, 0, &path.display().to_string(), 0.1, false).unwrap();
        let job = db::claim_pending(&mut conn).unwrap().unwrap();
        (meeting, job)
    }

    /// 会議の持ち主のキーだけが使われ、他人のキーや共通のキーに頼らない（#78）。
    #[tokio::test]
    async fn each_meeting_uses_its_owners_key() {
        let (base, seen) = mock_ai().await;
        std::env::set_var("KIKIMIMI_GEMINI_BASE", format!("{base}/v1beta"));
        std::env::set_var("KIKIMIMI_OPENAI_BASE", format!("{base}/v1"));
        std::env::set_var("KIKIMIMI_ANTHROPIC_BASE", format!("{base}/anthropic/v1"));
        let dir = std::env::temp_dir().join(format!("kikimimi-byok-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::new(db::open(&dir.join("t.sqlite")).unwrap(), dir);
        let (alice, bob, carol) = {
            let conn = state.db().lock().await;
            let hash = crate::accounts::hash_password("password-for-test").unwrap();
            let alice = crate::accounts::create_user(&conn, "alice", &hash).unwrap().unwrap();
            let bob = crate::accounts::create_user(&conn, "bob", &hash).unwrap().unwrap();
            let carol = crate::accounts::create_user(&conn, "carol", &hash).unwrap().unwrap();
            db::patch_user_settings(&conn, alice, db::SettingsPatch { api_key: Some("key-alice".into()), ..Default::default() }).unwrap();
            db::patch_user_settings(
                &conn,
                bob,
                db::SettingsPatch {
                    provider: Some("openai".into()),
                    openai_api_key: Some("sk-bob".into()),
                    openai_model: Some("gpt-5-mini".into()),
                    openai_transcribe_model: Some("gpt-4o-transcribe".into()),
                    // B が Gemini のキーも持っていても、OpenAI を選んでいれば使わない
                    api_key: Some("key-bob-gemini".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            (alice, bob, carol)
        };
        let (meeting_a, job_a) = queue_chunk(&state, alice, "A").await;
        process_audio(&state, job_a).await;
        let (meeting_b, job_b) = queue_chunk(&state, bob, "B").await;
        process_audio(&state, job_b).await;
        let (_, job_c) = queue_chunk(&state, carol, "C").await;
        process_audio(&state, job_c).await;

        let calls = seen.lock().unwrap().clone();
        let gemini: Vec<_> = calls.iter().filter(|(path, _)| path.contains("generateContent")).collect();
        let openai: Vec<_> = calls.iter().filter(|(path, _)| path.starts_with("/v1/")).collect();
        assert_eq!(gemini.len(), 1, "{calls:?}");
        assert_eq!(gemini[0].1, "key-alice");
        assert_eq!(openai.len(), 2, "音声認識とチャットの 2 回: {calls:?}");
        assert!(openai.iter().all(|(_, key)| key == "Bearer sk-bob"), "{calls:?}");
        assert!(!calls.iter().any(|(_, key)| key.contains("key-bob-gemini")));

        let conn = state.db().lock().await;
        assert_eq!(db::list_segments(&conn, meeting_a).unwrap()[0].text, "Gemini の書き起こし");
        assert_eq!(db::list_segments(&conn, meeting_b).unwrap()[0].text, "OpenAI の書き起こし");
        // キーの無い C は、他人のキーを借りずに失敗として残る。
        assert_eq!(calls.len(), 3);
        let errors: Vec<String> = conn
            .prepare("SELECT error FROM audio_jobs WHERE status='error'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("APIキーを登録"), "{errors:?}");
        drop(conn);

        // 要約も、選んでいる提供元の本人のキーで作る。OpenAI を選んだ会員に Gemini のキーを求めない。
        {
            // A は人が参加者を入れている。B は空欄なので発言から拾う。
            let conn = state.db().lock().await;
            db::update_meeting_info(&conn, meeting_a, None, Some("人が入れた参加者")).unwrap();
        }
        let before = seen.lock().unwrap().len();
        summarize(&state, meeting_b).await;
        summarize(&state, meeting_a).await;
        let calls = seen.lock().unwrap()[before..].to_vec();
        assert_eq!(calls.len(), 3, "B は参加者と要約の 2 回、A は要約の 1 回: {calls:?}");
        assert!(calls.iter().any(|(path, key)| path == "/v1/chat/completions" && key == "Bearer sk-bob"), "{calls:?}");
        assert!(calls.iter().any(|(path, key)| path.contains("generateContent") && key == "key-alice"), "{calls:?}");
        let conn = state.db().lock().await;
        let b = db::get_meeting(&conn, meeting_b).unwrap().unwrap();
        assert_eq!(b.status, "done");
        assert!(b.error.is_none(), "{:?}", b.error);
        assert!(!b.summary_md.is_empty());
        assert_eq!(b.participants, "鈴木", "空欄だった B は発言から拾う");
        assert!(b.participants_auto);
        let a = db::get_meeting(&conn, meeting_a).unwrap().unwrap();
        assert_eq!(a.participants, "人が入れた参加者", "人が入れた参加者は上書きしない");
        drop(conn);

        // 作り直しでは、自動で入れた参加者を拾い直す。人が直したら、以後は拾い直さない。
        {
            let conn = state.db().lock().await;
            db::set_auto_participants(&conn, meeting_b, "古い自動の名前").unwrap();
        }
        summarize(&state, meeting_b).await;
        {
            let conn = state.db().lock().await;
            assert_eq!(db::get_meeting(&conn, meeting_b).unwrap().unwrap().participants, "鈴木");
            db::update_meeting_info(&conn, meeting_b, None, Some("人が直した")).unwrap();
        }
        let before = seen.lock().unwrap().len();
        summarize(&state, meeting_b).await;
        let calls = seen.lock().unwrap()[before..].to_vec();
        assert_eq!(calls.len(), 1, "人が直したあとは要約だけ: {calls:?}");
        let conn = state.db().lock().await;
        let b = db::get_meeting(&conn, meeting_b).unwrap().unwrap();
        assert_eq!(b.participants, "人が直した");
        assert!(!b.participants_auto);
        drop(conn);

        // Claude を選んだ会員: 文字起こしは選んだ OpenAI、回答・参加者・要約は Claude（#85）。
        let dave = {
            let conn = state.db().lock().await;
            let hash = crate::accounts::hash_password("password-for-test").unwrap();
            let dave = crate::accounts::create_user(&conn, "dave", &hash).unwrap().unwrap();
            db::patch_user_settings(
                &conn,
                dave,
                db::SettingsPatch {
                    provider: Some("anthropic".into()),
                    anthropic_api_key: Some("sk-ant-dave".into()),
                    anthropic_model: Some("claude-opus-5-5".into()),
                    transcriber: Some("openai".into()),
                    openai_api_key: Some("sk-dave".into()),
                    openai_model: Some("gpt-5-mini".into()),
                    openai_transcribe_model: Some("gpt-4o-transcribe".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            dave
        };
        let before = seen.lock().unwrap().len();
        let (meeting_d, job_d) = queue_chunk(&state, dave, "D").await;
        process_audio(&state, job_d).await;
        summarize(&state, meeting_d).await;
        let calls = seen.lock().unwrap()[before..].to_vec();
        let paths: Vec<&str> = calls.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            ["/v1/audio/transcriptions", "/anthropic/v1/messages", "/anthropic/v1/messages", "/anthropic/v1/messages"],
            "文字起こし → 回答（Claude）→ 参加者 → 要約: {calls:?}"
        );
        assert_eq!(calls[0].1, "Bearer sk-dave");
        let claude: Vec<&String> = calls[1..].iter().map(|(_, note)| note).collect();
        assert!(claude.iter().all(|note| note.starts_with("sk-ant-dave|2026-10-21|server-side-fallback-2026-07-01|fallbacks=default")), "{claude:?}");
        assert!(claude[0].ends_with("format=json_schema"), "回答は構造化出力: {claude:?}");
        assert!(claude[2].ends_with("format="), "要約は文章: {claude:?}");
        assert!(!calls.iter().any(|(_, key)| key.contains("key-alice") || key.contains("sk-bob")));
        let conn = state.db().lock().await;
        assert_eq!(db::list_segments(&conn, meeting_d).unwrap()[0].text, "OpenAI の書き起こし");
        let d = db::get_meeting(&conn, meeting_d).unwrap().unwrap();
        assert_eq!(d.status, "done");
        assert!(d.error.is_none(), "{:?}", d.error);
        assert!(d.summary_md.contains("Claude の要約"), "{}", d.summary_md);
        assert_eq!(d.participants, "高橋");
    }
}
