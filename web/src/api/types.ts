// docs/api.md の型。サーバの JSON をそのまま表す。

export type Plan = { code: string; name: string; registrable?: boolean; features?: { realtime_captions?: boolean } };

export type User = { id: number; login_id: string; created_at: string; plan: Plan };

export type AuthResponse = { user: User; csrf: string };

export type UsageItem = {
  item: string;
  label: string;
  used: number | null;
  limit: number | null;
  unit: string;
  period_end: string | null;
};

export type Account = { user: User; plan: Plan; usage: UsageItem[]; note?: string };

export type ModelCheck = { running: boolean; error: string | null };

export type IndexStats = { docs: number; chunks: number; errors: number };

export type Upload = { name: string; bytes: number; uploaded_at: string };

export type Provider = "gemini" | "openai" | "anthropic";

/** 文字起こしができる提供元（Claude は音声を受け付けない）。 */
export type Transcriber = "gemini" | "openai";

export type Settings = {
  provider: Provider;
  api_key_set: boolean;
  openai_key_set: boolean;
  openai_model: string;
  openai_transcribe_model: string;
  anthropic_key_set: boolean;
  anthropic_model: string;
  transcriber: Transcriber;
  /** リアルタイム文字起こし（速報字幕, #104）。 */
  realtime_captions: "off" | "local" | "cloud";
  /** いまのプランで使えるか。 */
  realtime_allowed: boolean;
  /** 音声を Google に送る認識を運用者が許可しているか。 */
  cloud_speech_allowed: boolean;
  model: string;
  model_auto: boolean;
  model_check: ModelCheck;
  summary_prompt: string;
  default_summary_prompt: string;
  knowledge_path: string;
  knowledge_root: string;
  fess_url: string;
  fess_token_set: boolean;
  fess_enabled: boolean;
  index: IndexStats;
  index_errors: { path: string; error: string }[];
  uploads: Upload[];
};

export type Template = { id: number; name: string; questions: string[] };

export type MeetingStatus = "draft" | "live" | "paused" | "wrapping" | "done";

export type Meeting = {
  id: number;
  title: string;
  status: MeetingStatus;
  created_at: string;
  started_at: string | null;
  ended_at: string | null;
  summary_md: string;
  summary_at: string | null;
  duration_sec: number;
  finish_requested: boolean;
  active_question_id: number | null;
  error: string | null;
  participants: string;
  /** 参加者は要約のときに発言から自動で拾ったもの（人が編集すると false）。 */
  participants_auto?: boolean;
  folder_id: number | null;
  position: number;
  /** 会議中に AI が出した「次に聞くとよいこと」（「[切り口] 問いかけ」）。 */
  next_hint: string;
  pending_jobs: number;
  retry_waiting: number;
  next_retry_at: string | null;
  failed_jobs: number;
};

export type Evidence = { segment_id: number; start_sec: number; quote: string };

export type Question = {
  id: number;
  position: number;
  body: string;
  status: "open" | "draft" | "sufficient" | "skipped";
  answer: string;
  locked: boolean;
  source: "ai" | "human" | null;
  evidence: Evidence[];
  /** この議題を足したのは誰か。 */
  origin: "human" | "ai";
  /** 「今回は聞かない」にしたのは誰か（空なら外していない）。 */
  skipped_by: "" | "human" | "ai";
  skip_reason: string;
  /** 議題の目安時間（分）。 */
  minutes: number | null;
  /** いまの議題として話していた時間（録音時間の秒、確定分）。 */
  spent_sec: number;
  /** いまの議題になった録音時間（秒）。いまの議題でなければ null。 */
  active_since: number | null;
};

export type NoteKind = "decision" | "todo" | "number" | "concern" | "term";

/** 会議中に AI が発言から拾ったメモ。 */
export type Note = { id: number; kind: NoteKind; text: string; detail: string; start_sec: number | null };

/** 「ここ重要」の印（録音時間の秒）。 */
export type Mark = { id: number; at_sec: number };

/** 会議に添付した顧客資料（#97）。 */
export type MeetingDoc = { id: number; name: string; bytes: number; chunks: number; error: string | null; created_at: string };

export type Folder = { id: number; name: string; position: number; meetings: number };

/** 書き起こしの区間。speaker は会議の終了後の推定（#83）か、人が付けた話者。 */
export type Segment = { id: number; start_sec: number; text: string; speaker?: string };

export type Hit = {
  id: number;
  source: "local" | "fess" | "customer";
  title: string;
  locator: string;
  snippet: string;
  query: string;
  pinned: boolean;
};

export type MeetingLimits = { minutes_used: number; minutes_limit: number | null };

export type MeetingDetail = {
  meeting: Meeting;
  questions: Question[];
  segments: Segment[];
  hits: Hit[];
  notes: Note[];
  marks: Mark[];
  docs?: MeetingDoc[];
  errors: string[];
  summarizing: boolean;
  limits: MeetingLimits;
};

export type MeetingList = { total: number; done: number; meetings: Meeting[] };

export type ExportScope = "summary" | "qa" | "full";

export type SseEvent =
  | { type: "hello" }
  | { type: "resync" }
  | { type: "segment"; segment: Segment; edited?: boolean }
  | { type: "questions"; questions: Question[]; active_question_id: number | null }
  | { type: "knowledge"; hits: Hit[] }
  | { type: "notes"; notes: Note[] }
  | { type: "marks"; marks: Mark[] }
  | { type: "hint"; next_hint: string }
  | {
      type: "jobs";
      pending: number;
      retry_waiting?: number;
      next_retry_at?: string | null;
      failed?: number;
      errors?: string[];
    }
  | { type: "meeting"; meeting: Meeting }
  | { type: "status"; status?: MeetingStatus; meeting?: Meeting; summary_md?: string; error?: string | null }
  | { type: "summarizing"; running: boolean }
  | { type: "warning"; message: string }
  | { type: "deleted" }
  | { type: "limit"; limit: UsageItem };
