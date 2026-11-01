use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::{broadcast, Mutex, Notify};

struct Inner {
    db: Mutex<Connection>,
    db_path: Option<PathBuf>,
    data_dir: PathBuf,
    events: Mutex<HashMap<i64, broadcast::Sender<String>>>,
    notify: Notify,
    http: reqwest::Client,
    summarizing: std::sync::Mutex<HashSet<i64>>,
    model_checks: std::sync::Mutex<HashMap<i64, ModelCheck>>,
    attempts: std::sync::Mutex<HashMap<String, (u32, Instant)>>,
}

/// 設定画面に出す、モデル自動選択の進み具合。会員ごとに持つ。
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ModelCheck {
    pub running: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    pub fn new(db: Connection, data_dir: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .expect("http client");
        let db_path = db.path().filter(|path| !path.is_empty()).map(PathBuf::from);
        Self {
            inner: Arc::new(Inner {
                db: Mutex::new(db),
                db_path,
                data_dir,
                events: Mutex::new(HashMap::new()),
                notify: Notify::new(),
                http,
                summarizing: std::sync::Mutex::new(HashSet::new()),
                model_checks: std::sync::Mutex::new(HashMap::new()),
                attempts: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.inner.data_dir
    }

    pub fn db(&self) -> &Mutex<Connection> {
        &self.inner.db
    }

    /// 書き込み用の接続とは別の、読み取り専用の接続を開く。ファイルの DB のときだけ。
    pub fn reader(&self) -> Option<Connection> {
        let path = self.inner.db_path.as_ref()?;
        crate::db::open_reader(path).ok()
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub fn notify_jobs(&self) {
        self.inner.notify.notify_one();
    }

    pub async fn wait_jobs(&self) {
        self.inner.notify.notified().await;
    }

    /// 同じ会議の要約を同時に二つ走らせない。取れたら true。
    pub fn begin_summary(&self, meeting_id: i64) -> bool {
        self.inner
            .summarizing
            .lock()
            .expect("summarizing")
            .insert(meeting_id)
    }

    pub fn end_summary(&self, meeting_id: i64) {
        self.inner
            .summarizing
            .lock()
            .expect("summarizing")
            .remove(&meeting_id);
    }

    pub fn is_summarizing(&self, meeting_id: i64) -> bool {
        self.inner
            .summarizing
            .lock()
            .expect("summarizing")
            .contains(&meeting_id)
    }

    pub fn model_check(&self, user_id: i64) -> ModelCheck {
        self.inner
            .model_checks
            .lock()
            .expect("model_checks")
            .get(&user_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_model_check(&self, user_id: i64, check: ModelCheck) {
        self.inner
            .model_checks
            .lock()
            .expect("model_checks")
            .insert(user_id, check);
    }

    /// 回数の制限。`window` の間に `max` 回を超えたら、待つべき秒数を返す。
    pub fn throttle(&self, key: &str, max: u32, window: Duration) -> Result<(), u64> {
        let mut map = self.inner.attempts.lock().expect("attempts");
        let now = Instant::now();
        if map.len() > 10_000 {
            map.retain(|_, (_, start)| now.duration_since(*start) < window);
        }
        let entry = map.entry(key.to_string()).or_insert((0, now));
        if now.duration_since(entry.1) >= window {
            *entry = (0, now);
        }
        if entry.0 >= max {
            let wait = window.saturating_sub(now.duration_since(entry.1));
            return Err(wait.as_secs().max(1));
        }
        entry.0 += 1;
        Ok(())
    }

    /// ログインに成功したら、そのIDの失敗回数を戻す。
    pub fn forget(&self, key: &str) {
        self.inner.attempts.lock().expect("attempts").remove(key);
    }

    pub async fn publish(&self, meeting_id: i64, payload: serde_json::Value) {
        let tx = {
            let mut map = self.inner.events.lock().await;
            map.retain(|_, tx| tx.receiver_count() > 0);
            map.get(&meeting_id).cloned()
        };
        if let Some(tx) = tx {
            let _ = tx.send(payload.to_string());
        }
    }

    pub async fn subscribe(&self, meeting_id: i64) -> broadcast::Receiver<String> {
        let mut map = self.inner.events.lock().await;
        let tx = map
            .entry(meeting_id)
            .or_insert_with(|| broadcast::channel(256).0)
            .clone();
        tx.subscribe()
    }
}
