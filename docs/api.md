# API 契約（会員制・v4）

画面（`web/`、React + TypeScript + Vite）とサーバ（`src/`、Rust / Axum）の取り決め。
すべて同一オリジン。JSON は UTF-8。時刻は UTC の RFC 3339（`2026-10-31T12:00:00Z`）。

## 認証とセッション

- ログインすると `kikimimi_session` Cookie（HttpOnly、SameSite=Lax、HTTPS 時は Secure）が付く。`fetch` は既定の `credentials: "same-origin"` のままでよい。
- `GET` 以外の `/api/*`（`/api/auth/register`、`/api/auth/register/code`、`/api/auth/register/verify`、`/api/auth/login`、`/api/auth/password/forgot`、`/api/auth/password/reset` を除く）は、ヘッダ `X-CSRF-Token: <csrf>` が必須。`csrf` は登録・ログイン・再設定・`/api/auth/me` の応答に入る。
- 未ログインの `/api/*` は `401 {"error": "...", "code": "unauthenticated"}`。CSRF 不一致は `403 {"code": "csrf"}`。
- 共通トークン（旧 `APP_TOKEN`）と共有パスワードは廃止。HTML に秘密値は埋め込まない。

| メソッド | パス | 本文 | 応答 |
|---|---|---|---|
| POST | `/api/auth/register/code` | `{"login_id","invite"}` | `200 {"sent": true}`。招待コードが一致したとき、`login_id` のメールアドレスへ6桁の確認コードを送る |
| POST | `/api/auth/register/verify` | `{"login_id","code"}` | `200 {"ok": true}`。6桁が一致すると、パスワード入力へ進める |
| POST | `/api/auth/register` | `{"login_id","password","code","invite"}` | `201 {"user": User, "csrf"}`。利用規約への同意のあと、招待コードと確認コードが一致したときだけ会員を作る |
| POST | `/api/auth/password/forgot` | `{"login_id"}` | `200 {"sent": true}`。登録済みのメールアドレスなら再設定用の確認コードを送る |
| POST | `/api/auth/password/reset` | `{"login_id","code","password"}` | `200 {"user": User, "csrf"}`。確認コードが一致したときパスワードを替え、ログインする |
| POST | `/api/auth/login` | `{"login_id","password"}` | `200 {"user": User, "csrf"}` |
| POST | `/api/auth/logout` | なし | `204` |
| GET | `/api/auth/me` | なし | `200 {"user": User, "csrf"}` / `401` |
| POST | `/api/auth/password` | `{"current_password","new_password"}` | `204`（他のセッションは失効） |

- `User` = `{"id": number, "login_id": string, "created_at": string, "plan": {"code": "free", "name": "Free"}}`
- 新規のログインIDはメールアドレス（大文字は小文字にそろえる）。確認コードは6桁、10分間、同じコードは5回まで。会員制以前からのログインIDは、そのままログインに使える。
- 暫定の招待コードは `docomo2`（前後の空白は無視、大文字小文字は区別）。違うと `invalid_invite`。
- パスワード: 8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）。
- エラーコード: `invalid_login_id` / `weak_password` / `invalid_invite` / `login_taken`（409）/ `invalid_code` / `code_expired` / `code_locked` / `password_changed` / `mail_unconfigured` / `mail_failed` / `invalid_credentials`（401）/ `rate_limited`（429、`retry_after` 秒）。
- 登録時にプランは指定できない（送っても無視し、常に Free）。
- パスワードを忘れた場合は、登録済みのメールアドレスへ再設定用の確認コードを送る。未登録のアドレスには送らない。運用者の `kikimimi admin reset-password <login_id>` も残している。

## アカウントと利用量

`GET /api/account`

```json
{
  "user": User,
  "plan": {"code": "free", "name": "Free", "registrable": true},
  "usage": [
    {"item": "daily_meetings", "label": "今日開始した会議", "used": 3, "limit": 30, "unit": "件", "period_end": "2026-11-02T15:00:00Z"},
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
- `realtime_captions`（`"off"` / `"local"` / `"cloud"`）: リアルタイム文字起こし（Chrome の速報字幕, #104）。`realtime_allowed` はプランの機能（`/api/account` の `plan.features.realtime_captions` と同じ）。使えないプランで `local` / `cloud` を PUT すると 403。`cloud` は `cloud_speech_allowed`（運用者の `KIKIMIMI_ALLOW_CLOUD_SPEECH=1`）のときだけ。
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
- `POST /api/meetings/:id/start` 本文は空、または `{"speech": "api"|"chrome"|"off", "transcript_only": bool}`。空なら従来どおり（speech は変えない。新しい会議の初期値は `api`）。`speech=api` で文字起こし用のキーが無いときは 400。
  - `api` は Chrome の速報（保存しない）と、API による文字起こし。`chrome` は Chrome の音声認識だけを使い、確定した文を `captions` に保存する（API は呼ばない）。`off` は録音だけ。
- `POST /api/meetings/:id/speech` `{"speech","transcript_only"}` → MeetingDetail。録音中・一時停止・下書きだけ。`api` にすると保留中の録音を文字起こし待ちに戻す。キーが無い `api` は 400。
- `POST /api/meetings/:id/captions` `{"start_sec","text"}` → `{"segment"}`。`speech=chrome` で録音中か一時停止中だけ保存する。近い時刻の区間があれば `segment` は null。
- `POST /api/meetings/:id/pause` / `resume` / `finish` / `cancel-finish` / `retry-audio` / `give-up-retries` / `resummarize` → MeetingDetail（`retry-audio` と `give-up-retries` は `{"released"}`、`resummarize` は `{"started": true}`）
  - `finish` の本文は空、または `{"close":"api"|"chrome"|"plain"}`。空は従来どおり（保留は起こさず、API で要約）。`api` は保留中の録音を文字起こししてから要約（文字起こし用キーが必要）。`chrome` はいまの書き起こしを API でまとめる（まとめるキーが無ければ `plain`）。`plain` は API を使わず書き起こしから議事録を組み立て、`error` は空のまま。
  - `start` と `resume` は、同時録音数と今月の会議数の上限で `403 limit_exceeded` になりうる。
  - `speech` が `chrome` または `off` の録音は `audio_jobs.status=held` になり、API を呼ばない。`held` は終了待ちの件数に入らない。
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
  speech: "api" | "chrome" | "off"; transcript_only: boolean;
  finish_close: "" | "api" | "chrome" | "plain";
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
- サーバは `KIKIMIMI_WEB_DIR`（既定: 実行ファイルの隣の `web`、無ければ `./web/dist`）から配信する。
