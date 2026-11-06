import { api, ApiError, limitText, request } from "../api/client";
import type { Meeting, MeetingDetail, Settings } from "../api/types";
import { fmt, fmtLocal, nowTitle, statusLabel } from "../lib/format";
import { clearSessionData } from "../lib/prefs";
import { navigate } from "../lib/router";
import { openDialog } from "./dialog";
import { clearAllNotices, notify } from "./notices";
import { clearForm, formKey, setSetupReturn, type MeetingForm } from "./draft";
import { closeMeeting, currentDetail, setDetail } from "./meeting";
import { currentUser, leaveSession } from "./session";
import {
  drainQueue,
  dropQueue,
  ensureAudio,
  haltCapture,
  queuedFor,
  queueLength,
  recorderState,
  recordingHere,
  resetAll,
  sentSamples,
  startCapturing,
  stopCapturing,
  stopStream,
  useSavedTabPref,
} from "../recorder/recorder";

export function providerName(provider: Settings["provider"] | undefined): string {
  if (provider === "openai") return "OpenAI";
  if (provider === "anthropic") return "Claude";
  return "Gemini";
}

/** その提供元のキーが登録済みか。 */
export function keySet(settings: Settings, provider: Settings["provider"]): boolean {
  if (provider === "openai") return !!settings.openai_key_set;
  if (provider === "anthropic") return !!settings.anthropic_key_set;
  return !!settings.api_key_set;
}

/** 録音を文字にするキーがあるか。Claude のときは文字起こし用の提供元を見る。 */
export function transcribeReady(settings: Settings): boolean {
  const ears = settings.provider === "anthropic" ? settings.transcriber || "gemini" : settings.provider;
  return keySet(settings, ears);
}

/** 書き起こしを議事録にまとめるキーがあるか。 */
export function chatReady(settings: Settings): boolean {
  return keySet(settings, settings.provider);
}

/** 会議に要るキーがそろっているか。Claude のときは文字起こしに使う提供元のキーも要る。 */
export function keyReady(settings: Settings): boolean {
  return chatReady(settings) && transcribeReady(settings);
}

/** 足りないキーの提供元名（会議の開始前の案内に使う）。 */
export function missingKeyName(settings: Settings): string {
  if (settings.provider === "anthropic" && keySet(settings, "anthropic")) return `文字起こしに使う ${providerName(settings.transcriber || "gemini")}`;
  return providerName(settings.provider);
}

// 送り切れないときは、待つか破棄するかを利用者に選んでもらう。
export async function drainOrAsk(): Promise<boolean> {
  while (!(await drainQueue())) {
    const choice = await openDialog({
      title: "録音の送信が終わりません",
      body: <p>未送信の録音が {queueLength()} 区間あります。ネットワークかサーバの状態を確認してください。</p>,
      confirm: "待つ",
      cancel: "操作をやめる",
      extra: [{ label: "未送信分を破棄して進む", value: "drop" }],
    });
    if (!choice) return false;
    if (choice._choice === "drop") {
      dropQueue();
      return true;
    }
  }
  return true;
}

/** 空の議題を除き、目安時間（分）の並びをそろえる。 */
function cleanAgenda(form: MeetingForm): { questions: string[]; agenda_minutes: (number | null)[] } {
  const questions: string[] = [];
  const agenda_minutes: (number | null)[] = [];
  form.questions.forEach((text, index) => {
    if (!text.trim()) return;
    questions.push(text.trim());
    const value = Number((form.minutes?.[index] || "").trim());
    agenda_minutes.push(Number.isFinite(value) && value > 0 ? Math.round(value) : null);
  });
  return { questions, agenda_minutes };
}

export async function saveMeeting(form: MeetingForm, existingId: number | null): Promise<MeetingDetail> {
  let title = form.title.trim();
  if (existingId == null && !title) title = nowTitle();
  if (existingId != null && !title) throw new Error("会議タイトルを入力してください");
  const body = { title, ...cleanAgenda(form), participants: form.participants };
  if (existingId == null) {
    const folder = form.folderId && /^\d+$/.test(form.folderId) ? Number(form.folderId) : null;
    return api<MeetingDetail>("/api/meetings", { method: "POST", body: { ...body, folder_id: folder } });
  }
  return api<MeetingDetail>(`/api/meetings/${existingId}`, { method: "PUT", body });
}

/** 会議を開始する。APIキーが無ければ、入力を残したまま設定へ案内する。 */
export async function beginMeeting(form: MeetingForm, existingId: number | null) {
  const rec = recorderState.get();
  if (rec.capturing && rec.meetingId !== existingId) {
    throw new Error("録音中の会議があります。先に一時停止か終了をしてください");
  }
  const user = currentUser();
  if (!user) throw new Error("ログインしてください");
  const settings = await api<Settings>("/api/settings");
  if (form.speech === "api" && !transcribeReady(settings)) {
    setSetupReturn(existingId == null ? "#/new" : `#/m/${existingId}`);
    notify(`先に ${missingKeyName(settings)} の APIキーを登録してください。登録すると、入力した会議の準備に戻れます。Chrome の音声認識なら、キーがなくても始められます。`, "warn", "setup");
    navigate("#/settings");
    return;
  }
  let started: MeetingDetail;
  try {
    // テスト中のマイクを止め、選んだマイクとタブ音声で開き直す。
    if (!rec.capturing) stopStream();
    await ensureAudio();
    const saved = await saveMeeting(form, existingId);
    started = await api<MeetingDetail>(`/api/meetings/${saved.meeting.id}/start`, {
      method: "POST",
      body: { speech: form.speech, transcript_only: form.speech === "api" && form.transcriptOnly },
    });
  } catch (err) {
    // 開始できなかったのにマイクを掴み続けない。
    if (!recorderState.get().capturing) stopStream();
    throw err;
  }
  clearForm(formKey(existingId));
  const id = started.meeting.id;
  startCapturing(id, user.id, 0);
  setDetail(started);
  navigate(`#/m/${id}`);
}

/**
 * 会議を開始するが、録音はしない（一時停止のまま）。マイクは開かない（#101）。
 * あとから「再開」で録音を始めるか、「取り込む…」でボイスレコーダーの録音を足す。
 */
export async function beginMeetingPaused(form: MeetingForm, existingId: number | null) {
  if (!currentUser()) throw new Error("ログインしてください");
  const settings = await api<Settings>("/api/settings");
  if (form.speech === "api" && !transcribeReady(settings)) {
    setSetupReturn(existingId == null ? "#/new" : `#/m/${existingId}`);
    notify(`先に ${missingKeyName(settings)} の APIキーを登録してください。登録すると、入力した会議の準備に戻れます。Chrome の音声認識なら、キーがなくても始められます。`, "warn", "setup");
    navigate("#/settings");
    return;
  }
  const saved = await saveMeeting(form, existingId);
  await api(`/api/meetings/${saved.meeting.id}/start`, {
    method: "POST",
    body: { speech: form.speech, transcript_only: form.speech === "api" && form.transcriptOnly },
  });
  const paused = await api<MeetingDetail>(`/api/meetings/${saved.meeting.id}/pause`, { method: "POST" });
  clearForm(formKey(existingId));
  setDetail(paused);
  navigate(`#/m/${saved.meeting.id}`);
  notify("会議を開始しました（録音は一時停止中です）。録音するときは「再開」を押してください。", "info");
}

export async function pauseMeeting(id: number) {
  if (recordingHere(id)) {
    // 一時停止中はマイクを離す。再開のときに取り直す。
    haltCapture();
    if (!(await drainOrAsk())) return;
  }
  setDetail(await api<MeetingDetail>(`/api/meetings/${id}/pause`, { method: "POST" }));
}

export async function resumeMeeting(id: number) {
  const rec = recorderState.get();
  const user = currentUser();
  if (!user) throw new Error("ログインしてください");
  if (rec.capturing && rec.meetingId !== id) {
    throw new Error("別の会議を録音中です。そちらを一時停止か終了してから再開してください");
  }
  if (Object.keys(rec.queued).some((key) => Number(key) !== id)) {
    throw new Error("別の会議の録音を送信中です。送り終わってから再開してください");
  }
  useSavedTabPref();
  const open = currentDetail();
  if (open && open.meeting.id === id && (open.meeting.speech || "api") === "api") {
    const settings = await api<Settings>("/api/settings");
    if (!transcribeReady(settings)) {
      notify("API の文字起こしにはキーが必要です。録音中の切り替えで Chrome の音声認識にするか、設定でキーを登録してください。", "warn");
      navigate("#/settings");
      return;
    }
  }
  await ensureAudio();
  let detail: MeetingDetail;
  try {
    detail = await api<MeetingDetail>(`/api/meetings/${id}/resume`, { method: "POST" });
  } catch (err) {
    if (!recorderState.get().capturing) stopStream();
    throw err;
  }
  // 続きの区間は、サーバが受け取った録音の長さから始める。既存の区間を上書きしない。
  const already = queuedFor(id) ? sentSamples() : Math.round((detail.meeting.duration_sec || 0) * 16000);
  startCapturing(id, user.id, already);
  setDetail(detail);
}

async function finishKeys(): Promise<{ transcribe: boolean; chat: boolean }> {
  const settings = await api<Settings>("/api/settings");
  return { transcribe: transcribeReady(settings), chat: chatReady(settings) };
}

function finishOptions(keys: { transcribe: boolean; chat: boolean }): { value: string; label: string }[] {
  return [
    { value: "api", label: keys.transcribe ? "録音から API で文字起こしを確定し、要約する" : "録音から API で確定する（文字起こし用の APIキーが必要）" },
    { value: "chrome", label: keys.chat ? "いまの書き起こしを API で議事録にまとめる" : "書き起こしを API でまとめる（まとめる用の APIキーが必要）" },
    { value: "plain", label: "API を使わず、書き起こしを議事録にする" },
  ];
}

function defaultClose(detail: MeetingDetail | null, keys: { transcribe: boolean; chat: boolean }): string {
  const speech = detail?.meeting.speech || "api";
  if (speech === "api" && keys.transcribe) return "api";
  if (keys.chat) return "chrome";
  return "plain";
}

export async function finishMeeting(id: number, reason?: string) {
  // 聞き漏らしチェック: まだ終わっていない議題を、終える前に見せる。
  const current = currentDetail();
  const missing = current && current.meeting.id === id ? current.questions.filter((q) => q.status === "open" || (q.status === "draft" && !q.answer.trim())) : [];
  const keys = await finishKeys();
  const choice = await openDialog({
    title: missing.length ? `まだ終わっていない議題が ${missing.length} 件あります` : "会議を終了しますか？",
    body: (
      <>
        {reason ? <p className="banner">{reason}</p> : null}
        {missing.length ? (
          <div className="banner">
            <p>終える前に、次の議題を確かめておくと良いかもしれません。</p>
            <ul>
              {missing.slice(0, 8).map((q) => (
                <li key={q.id}>{q.body}</li>
              ))}
            </ul>
            <p className="small">扱わなくてよい議題は「今回は扱わない」で外せます。議事録には「次回確認すること」として残ります。</p>
          </div>
        ) : null}
        <p>終了のしかたを選んでください。録音は止まります。</p>
        <p className="muted">要約に取りかかる前なら「終了を取り消して録音に戻る」で戻せます。</p>
      </>
    ),
    fields: [
      {
        name: "close",
        label: "終了のしかた",
        type: "select",
        value: defaultClose(current, keys),
        options: finishOptions(keys),
      },
    ],
    confirm: "終了する",
    cancel: missing.length ? "会議に戻って聞く" : undefined,
    danger: true,
  });
  if (!choice) return;
  const close = choice.close === "chrome" && !keys.chat ? "plain" : choice.close;
  if (choice.close === "api" && !keys.transcribe) {
    notify("音声認識の確定には、文字起こし用のAPIキーが必要です。", "warn");
    navigate("#/settings");
    return;
  }
  if (recordingHere(id)) {
    haltCapture();
    if (!(await drainOrAsk())) return;
  }
  setDetail(await api<MeetingDetail>(`/api/meetings/${id}/finish`, { method: "POST", body: { close } }));
  const done =
    close === "plain"
      ? "録音を止めました。API は使わず、書き起こしから議事録を組み立てます。"
      : close === "chrome"
        ? "録音を止めました。いまの書き起こしを API で議事録にまとめています。"
        : "録音を止めました。録音から文字起こしを確定し、要約を進めています。";
  notify(done, "info");
}

export async function deleteMeeting(meeting: Meeting) {
  const recording = recordingHere(meeting.id);
  const active = ["live", "paused", "wrapping"].includes(meeting.status);
  const choice = await openDialog({
    title: `「${meeting.title}」を削除しますか？`,
    body: (
      <>
        {recording ? (
          <p className="banner error">この会議は録音中です。削除すると録音を止め、未送信分も含めて破棄します。</p>
        ) : active ? (
          <p className="banner">この会議は終了していません（{statusLabel(meeting.status)}）。</p>
        ) : null}
        <p>録音・書き起こし・要約・ピン留めがすべて削除され、元に戻せません。</p>
        <p className="muted">
          {fmtLocal(meeting.started_at || meeting.created_at)} ・ {statusLabel(meeting.status)} ・ 録音 {fmt(meeting.duration_sec)}
        </p>
      </>
    ),
    confirm: "削除する",
    danger: true,
  });
  if (!choice) return false;
  if (recording) stopCapturing("会議を削除したため、録音を止めました。");
  dropQueue(meeting.id);
  await api(`/api/meetings/${meeting.id}`, { method: "DELETE" });
  clearForm(formKey(meeting.id));
  if (currentDetail()?.meeting.id === meeting.id) closeMeeting();
  notify(`「${meeting.title}」を削除しました。`, "info");
  return true;
}

/** 上限に当たって録音を止めたとき、終了して要約するかを聞く。 */
export async function askFinishAfterLimit(meetingId: number, text: string) {
  const detail = currentDetail();
  if (!detail || detail.meeting.id !== meetingId || detail.meeting.status === "done") return;
  await finishMeeting(meetingId, `${text}受け付け済みの録音は保存されています。`).catch((err) => notify(err.message, "error"));
}

export function limitMessage(err: unknown): string | null {
  if (err instanceof ApiError && err.code === "limit_exceeded" && err.limit) return limitText(err.limit);
  return null;
}

/** ログアウト。録音中・未送信があれば確かめ、画面に残した個人の情報をすべて消す。 */
export async function logout() {
  const rec = recorderState.get();
  if (rec.capturing || rec.queueLength) {
    const ok = await openDialog({
      title: "録音中です。ログアウトしますか？",
      body: (
        <p>
          {rec.capturing ? "録音を止めます。" : ""}
          {rec.queueLength ? `未送信の録音 ${rec.queueLength} 区間は破棄されます。` : ""}
          ログアウトすると、この端末に残した会議の準備内容も消えます。
        </p>
      ),
      confirm: "ログアウトする",
      danger: true,
    });
    if (!ok) return;
  }
  resetAll();
  closeMeeting();
  try {
    await request("/api/auth/logout", { method: "POST" });
  } catch {
    // すでに切れていても、画面側は消す
  }
  clearSessionData();
  clearAllNotices();
  leaveSession();
  navigate("#/login");
}

export async function exportMarkdown(id: number, scope: string) {
  const response = await request(`/api/meetings/${id}/export?scope=${scope}`);
  const disposition = response.headers.get("content-disposition") || "";
  const encoded = disposition.match(/filename\*=UTF-8''([^;]+)/);
  const name = encoded ? decodeURIComponent(encoded[1]) : "kikimimi.md";
  const blob = new Blob([await response.text()], { type: "text/markdown" });
  const link = document.createElement("a");
  const url = URL.createObjectURL(blob);
  link.href = url;
  link.download = name;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export async function copyText(text: string, done: string) {
  try {
    await navigator.clipboard.writeText(text);
    notify(done, "info");
  } catch {
    throw new Error("クリップボードにコピーできませんでした。");
  }
}
