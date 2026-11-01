# API 契約（会員制・v4）

画面（`web/`、React + TypeScript + Vite）とサーバ（`src/`、Rust / Axum）の取り決め。
すべて同一オリジン。JSON は UTF-8。時刻は UTC の RFC 3339（`2026-10-31T12:00:00Z`）。

## 認証とセッション

- ログインすると `gijiroku_session` Cookie（HttpOnly、SameSite=Lax、HTTPS 時は Secure）が付く。`fetch` は既定の `credentials: "same-origin"` のままでよい。
- `GET` 以外の `/api/*`（`/api/auth/register` と `/api/auth/login` を除く）は、ヘッダ `X-CSRF-Token: <csrf>` が必須。`csrf` は登録・ログイン・`/api/auth/me` の応答に入る。
- 未ログインの `/api/*` は `401 {"error": "...", "code": "unauthenticated"}`。CSRF 不一致は `403 {"code": "csrf"}`。
- 共通トークン（旧 `APP_TOKEN`）と共有パスワードは廃止。HTML に秘密値は埋め込まない。

| メソッド | パス | 本文 | 応答 |
|---|---|---|---|
| POST | `/api/auth/register` | `{"login_id","password"}` | `201 {"user": User, "csrf"}` |
| POST | `/api/auth/login` | `{"login_id","password"}` | `200 {"user": User, "csrf"}` |
| POST | `/api/auth/logout` | なし | `204` |
| GET | `/api/auth/me` | なし | `200 {"user": User, "csrf"}` / `401` |
| POST | `/api/auth/password` | `{"current_password","new_password"}` | `204`（他のセッションは失効） |

- `User` = `{"id": number, "login_id": string, "created_at": string, "plan": {"code": "free", "name": "Free"}}`
- ログインID: 3〜32 文字、`a-z 0-9 _ . -`（大文字は小文字にそろえる）。予約語（`admin` など）は不可。
- パスワード: 8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）。
- エラーコード: `invalid_login_id` / `weak_password` / `login_taken`（409）/ `invalid_credentials`（401）/ `rate_limited`（429、`retry_after` 秒）。
- 登録時にプランは指定できない（送っても無視し、常に Free）。
- パスワードを忘れた場合はメールでの復旧はない。運用者がサーバで `gijiroku admin reset-password <login_id>` を実行する。

## アカウントと利用量

`GET /api/account`

```json
{
  "user": User,
  "plan": {"code": "free", "name": "Free", "registrable": true},
  "usage": [
    {"item": "monthly_meetings", "label": "今月開始した会議", "used": 3, "limit": 10, "unit": "件", "period_end": "2026-11-30T15:00:00Z"},
    {"item": "meeting_minutes", "label": "1 会議の録音時間", "used": null, "limit": 60, "unit": "分", "period_end": null},
    {"item": "storage_bytes", "label": "録音の保存容量", "used": 123456, "limit": 1073741824, "unit": "バイト", "period_end": null},
    {"item": "concurrent_recordings", "label": "同時に録音できる会議", "used": 0, "limit": 1, "unit": "件", "period_end": null}
  ],
  "note": "このサービスの Free プランと、Gemini / OpenAI / Claude の API 料金・利用枠は別です。…"
}
```

- `limit: null` は無制限。
- 上限に当たった操作は `403 {"error","code": "limit_exceeded", "limit": UsageItem}`（`used` は現在値）。Gemini の 429 やキー不正とは別のコード。

## 設定（本人だけ）

`GET /api/settings` / `PUT /api/settings`

```json
{
  "provider": "gemini",
  "api_key_set": true, "model": "gemini-3.8-flash", "model_auto": true,
  "openai_key_set": false, "openai_model": "", "openai_transcribe_model": "",
  "anthropic_key_set": false, "anthropic_model": "", "transcriber": "gemini",
  "realtime_captions": "off", "realtime_allowed": true, "cloud_speech_allowed": false,
  "model_check": {"running": false, "error": null},
  "summary_prompt": "...", "default_summary_prompt": "...",
  "knowledge_path": "", "knowledge_root": "/knowledge",
  "fess_url": "", "fess_token_set": false, "fess_enabled": false,
  "index": {"docs": 1, "chunks": 3, "errors": 0},
  "index_errors": [{"path": "...", "error": "..."}],
  "uploads": [{"name": "rule.md", "bytes": 120, "uploaded_at": "..."}]
}
```

- 回答候補・要約に使う AI は会員ごとに `provider`（`"gemini"` / `"openai"` / `"anthropic"`）で選ぶ。応答には `api_key_set`（Gemini）・`openai_key_set`・`anthropic_key_set`・`model`（Gemini）・`openai_model`（回答・要約）・`openai_transcribe_model`（音声認識）・`anthropic_model`（Claude）が入る。モデルは自動選択（Claude の既定は `claude-opus-5-5`）。
- Claude（`anthropic`）は音声を受け付けないため、文字起こしは `transcriber`（`"gemini"` / `"openai"`）の提供元で行い、その文を Claude に渡す。会議を始めるには Claude のキーと、`transcriber` の提供元のキーの両方が要る（要約と参加者の抽出は Claude のキーだけでできる）。
- Cursor は外部のアプリから呼べる文章生成の API を公開していないため、提供元に選べない（#85）。
- `realtime_captions`（`"off"` / `"local"` / `"cloud"`）: リアルタイム文字起こし（Chrome の速報字幕, #104）。`realtime_allowed` はプランの機能（`/api/account` の `plan.features.realtime_captions` と同じ）。使えないプランで `local` / `cloud` を PUT すると 403。`cloud` は `cloud_speech_allowed`（運用者の `GIJIROKU_ALLOW_CLOUD_SPEECH=1`）のときだけ。
- PUT の本文: `provider`, `transcriber`, `realtime_captions`, `api_key`（Gemini）, `openai_api_key`, `anthropic_api_key`, `summary_prompt`, `knowledge_path`, `fess_url`, `fess_token`, `fess_enabled`, `clear_api_key`, `clear_openai_api_key`, `clear_anthropic_api_key`, `clear_fess_token`（どれも省略可）。
- `knowledge_path` は運用者が許可したフォルダ（`knowledge_root`）の配下だけ。`knowledge_root` が空なら指定不可（アップロードだけ）。
- `POST /api/settings/check-api-key` `{"provider","api_key"}` → `{"ok","message"}`（`provider` は `gemini` / `openai` / `anthropic`、省略時は gemini）
- `POST /api/settings/check-fess` `{"fess_url","fess_token"}` → `{"ok","message"}`
- APIキーの値は応答に含めない。

### 資料のアップロード（本人の領域）

- `POST /api/knowledge/files` 本文は生のファイル、ヘッダ `X-File-Name: <URL エンコードしたファイル名>`。`.md` `.txt` `.pdf`、8MB まで。→ `{"uploads": [...]}`
- `DELETE /api/knowledge/files/:name` → `{"uploads": [...]}`
- `POST /api/knowledge/reindex` → `{"docs","chunks","errors","index_errors"}`（アップロードと `knowledge_path` の両方）
- `GET /api/knowledge/search?q=` → `{"hits": [HitDraft], "warning"}`

## テンプレート（本人だけ）

- `GET /api/templates` → `{"templates": [{"id","name","questions": [string]}]}`
- `POST /api/templates` `{"name","questions"}` / `PUT /api/templates/:id` / `DELETE /api/templates/:id`

## 会議（本人だけ。他人の ID は 404）

- `GET /api/meetings?q=&status=`（`status`: `""` / `active` / `draft` / `done`）→ `{"total","done","meetings": [Meeting]}`
- `POST /api/meetings` `{"title","questions","participants"}` → MeetingDetail
- `GET /api/meetings/:id` → MeetingDetail
- `PUT /api/meetings/:id`（下書きのみ）`{"title","questions","participants"}` / `PATCH /api/meetings/:id` `{"title"?, "participants"?}` / `DELETE /api/meetings/:id`
- `POST /api/meetings/:id/start` / `pause` / `resume` / `finish` / `cancel-finish` / `retry-audio` / `give-up-retries` / `resummarize` → MeetingDetail（`retry-audio` と `give-up-retries` は `{"released"}`、`resummarize` は `{"started": true}`）
  - `start` と `resume` は、同時録音数と今月の会議数の上限で `403 limit_exceeded` になりうる。
- `PUT /api/meetings/:id/summary` `{"summary_md"}` → MeetingDetail
- `POST /api/meetings/:id/questions` `{"body"}` → `{"questions"}`
- `PATCH /api/meetings/:id/questions/:qid` `{"body"?, "answer"?, "status"?, "confirmed"?}` → `{"questions"}`
- `DELETE /api/meetings/:id/questions/:qid` → `{"questions"}`
- `PATCH /api/meetings/:id/segments/:sid` `{"text"}` → `{"segment"}`
- `POST /api/meetings/:id/audio` 本文 WAV（16kHz / mono / 16bit）、ヘッダ `X-Start-Ms`, `X-Silent: 0|1` → `202 {"accepted","job_id","queued"}`。
  - `409`: 会議が録音を受け付けていない（終了・削除）→ 区間を捨てる。
  - `403 limit_exceeded`: 録音時間・保存容量の上限 → 区間を捨て、録音を止めて終了を促す。
- `GET /api/meetings/:id/audio`（Range 対応の WAV）
- `GET /api/meetings/:id/events`（SSE、下記）
- `GET /api/meetings/:id/export?scope=summary|qa|full` → Markdown（`Content-Disposition` に `filename*`）。`scope` 既定は `full`。
- `GET /api/meetings/:id/knowledge?q=` → `{"hits": [Hit], "warning"}`（見つかった資料を会議のナレッジに加える）
- `POST /api/meetings/:id/hits/:hit_id/pin` `{"pinned"}` → `{"hits"}`

### 型

```ts
type Meeting = {
  id: number; title: string; status: "draft" | "live" | "paused" | "wrapping" | "done";
  created_at: string; started_at: string | null; ended_at: string | null;
  summary_md: string; summary_at: string | null; duration_sec: number;
  finish_requested: boolean; active_question_id: number | null; error: string | null;
  participants: string; participants_auto: boolean; // 要約のときに発言から拾った参加者（人が編集すると false）
  pending_jobs: number; retry_waiting: number;
  next_retry_at: string | null; failed_jobs: number;
};
type Evidence = { segment_id: number; start_sec: number; quote: string };
type Question = {
  id: number; position: number; body: string;
  status: "open" | "draft" | "sufficient"; answer: string;
  locked: boolean;            // 人が確認・修正した（AI はもう上書きしない）
  source: "ai" | "human" | null; // 最後に回答を書いたのは誰か
  evidence: Evidence[];       // AI が回答に使った発言（新しい順。無ければ空）
};
type Segment = { id: number; start_sec: number; text: string };
type Hit = { id: number; source: "local" | "fess"; title: string; locator: string; snippet: string; query: string; pinned: boolean };
type MeetingDetail = {
  meeting: Meeting; questions: Question[]; segments: Segment[]; hits: Hit[];
  errors: string[]; summarizing: boolean;
  limits: { minutes_used: number; minutes_limit: number | null };
};
```

### SSE（`/api/meetings/:id/events`）

各イベントは `data: <JSON>`。

| type | 内容 |
|---|---|
| `hello` | 接続直後。2 回目以降の `hello` は再接続なので、詳細を取り直す |
| `resync` | 取りこぼし。詳細を取り直す |
| `segment` | `{"segment": Segment, "edited"?: true}` |
| `questions` | `{"questions": [Question], "active_question_id"}` |
| `knowledge` | `{"hits": [Hit]}` |
| `jobs` | `{"pending","retry_waiting","next_retry_at","failed","errors"}` |
| `meeting` | `{"meeting": Meeting}` |
| `status` | `{"status": "done", "meeting": Meeting, "summary_md", "error"}` |
| `summarizing` | `{"running": bool}` |
| `warning` | `{"message"}` |
| `deleted` | 会議が削除された |
| `limit` | `{"limit": UsageItem}` 録音の上限に近い・達した |

## 静的ファイル

- `/` とその他の `/api` 以外のパスは `web/dist/index.html`（ハッシュルーティング `#/...`）。
- `/assets/*` はビルド成果物。`/capture-worklet.js` は `web/public/` から。
- サーバは `GIJIROKU_WEB_DIR`（既定: 実行ファイルの隣の `web`、無ければ `./web/dist`）から配信する。
