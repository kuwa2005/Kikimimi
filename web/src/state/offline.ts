// オフラインで始める会議。回線が無い（またはサーバに届かない）ときでも録音を始め、
// 端末の保管庫に残す。つながったらサーバに会議を作って開始し、録音と書き起こしを送り、
// 番号を付け替えて、そのまま録音を続ける（録音は止めない）。

import { api, ApiError, isNetworkError } from "../api/client";
import type { MeetingDetail } from "../api/types";
import { createStore } from "../lib/store";
import { navigate } from "../lib/router";
import * as vault from "../recorder/vault";
import {
  ensureAudio,
  haltCapture,
  kickUploads,
  queuedFor,
  rebindLocal,
  recorderState,
  recordingElsewhere,
  recordingHere,
  restoreDone,
  resumeOffset,
  setLocalBinder,
  startCapturing,
  stopStream,
  pendingFor,
} from "../recorder/recorder";
import { notify } from "./notices";
import { currentUser, refreshSession } from "./session";
import type { MeetingForm } from "./draft";

/** この端末にだけある（まだサーバへ引き継ぎ終えていない）会議。 */
export const localMeetings = createStore<vault.VaultMeeting[]>([]);

async function reload() {
  try {
    const all = await vault.listMeetings();
    localMeetings.set(all.filter((m) => !m.serverId || !m.started || (m.finishClose != null && !finished.has(m.localId))));
  } catch {
    localMeetings.set([]);
  }
}

const finished = new Set<number>();

export function isLocalId(id: number) {
  return id < 0;
}

/** 画面の番号（正の数）と、端末内の番号（負の数）を行き来する。 */
export function localHash(localId: number) {
  return `#/local/${-localId}`;
}

function fallbackTitle(createdAt: number) {
  const d = new Date(createdAt);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `オフラインで録音した会議 ${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 回線が無いときに会議を始める。マイクだけ開き、端末に録音する。 */
export async function beginOfflineMeeting(form: MeetingForm) {
  const user = currentUser();
  if (!user) throw new Error("ログインしてください");
  const rec = recorderState.get();
  if (rec.capturing) throw new Error("録音中の会議があります。先に一時停止か終了をしてください");
  if (!rec.capturing) stopStream();
  await ensureAudio();
  const createdAt = Date.now();
  const questions: string[] = [];
  const agendaMinutes: (number | null)[] = [];
  form.questions.forEach((text, index) => {
    if (!text.trim()) return;
    questions.push(text.trim());
    const value = Number((form.minutes?.[index] || "").trim());
    agendaMinutes.push(Number.isFinite(value) && value > 0 ? Math.round(value) : null);
  });
  const meeting: vault.VaultMeeting = {
    localId: -createdAt,
    userId: user.id,
    title: form.title.trim() || fallbackTitle(createdAt),
    questions,
    agendaMinutes,
    participants: form.participants || "",
    speech: form.speech || "chrome",
    transcriptOnly: !!form.transcriptOnly,
    createdAt,
    serverId: null,
    started: false,
    finishClose: null,
    seconds: 0,
  };
  try {
    await vault.putMeeting(meeting);
  } catch {
    notify("この端末に会議を保存できません。録音はメモリにだけ残ります。ブラウザを閉じないでください。", "error", "vault");
  }
  startCapturing(meeting.localId, user.id, 0);
  await reload();
  navigate(localHash(meeting.localId));
  notify("オフラインで会議を始めました。録音はこの端末に保存し、つながったら自動でサーバへ送ります。", "info", "net");
  kickUploads();
}

export async function pauseLocal(localId: number) {
  if (recordingHere(localId)) haltCapture();
  const m = await vault.getMeeting(localId).catch(() => undefined);
  if (m) await vault.putMeeting({ ...m, seconds: resumeOffset(localId, m.seconds) / 16000 }).catch(() => {});
  await reload();
}

export async function resumeLocal(localId: number) {
  const user = currentUser();
  if (!user) throw new Error("ログインしてください");
  const rec = recorderState.get();
  if (rec.capturing && rec.meetingId !== localId) throw new Error("別の会議を録音中です");
  if (await recordingElsewhere(localId)) throw new Error("この会議は別のタブで録音中です");
  await restoreDone();
  const m = await vault.getMeeting(localId).catch(() => undefined);
  await ensureAudio();
  startCapturing(localId, user.id, resumeOffset(localId, m?.seconds || 0));
}

/** オフラインのまま終了する。送り終えたら、サーバで終了（議事録づくり）まで進める。 */
export async function finishLocal(localId: number) {
  if (recordingHere(localId)) haltCapture();
  const m = await vault.getMeeting(localId).catch(() => undefined);
  if (!m) return;
  const close = m.speech === "api" ? "api" : "chrome";
  await vault.putMeeting({ ...m, finishClose: close, seconds: resumeOffset(localId, m.seconds) / 16000 }).catch(() => {});
  await reload();
  notify("会議を終了しました。つながったら録音を送り、議事録を作ります。", "info", "net");
  kickUploads();
  void settle();
}

const binding = new Map<number, Promise<number | null>>();

/** オフラインの会議をサーバに作って開始する。作れたらサーバの会議番号、まだなら null。 */
function bind(localId: number): Promise<number | null> {
  const running = binding.get(localId);
  if (running) return running;
  const job = doBind(localId).finally(() => binding.delete(localId));
  binding.set(localId, job);
  return job;
}

async function doBind(localId: number): Promise<number | null> {
  if (!(await refreshSession())) return null;
  const user = currentUser();
  if (!user) return null;
  let m = await vault.getMeeting(localId).catch(() => undefined);
  if (!m) {
    // 会議の記録が無い区間（保存に失敗したなど）。日時だけの会議として引き継ぐ。
    m = {
      localId,
      userId: user.id,
      title: fallbackTitle(-localId),
      questions: [],
      agendaMinutes: [],
      participants: "",
      speech: "chrome",
      transcriptOnly: false,
      createdAt: -localId,
      serverId: null,
      started: false,
      finishClose: null,
      seconds: 0,
    };
  }
  if (m.userId !== user.id) return null;
  try {
    if (m.serverId == null) {
      const created = await api<MeetingDetail>("/api/meetings", {
        method: "POST",
        body: { title: m.title, questions: m.questions, agenda_minutes: m.agendaMinutes, participants: m.participants, folder_id: null },
      });
      m = { ...m, serverId: created.meeting.id };
      await vault.putMeeting(m).catch(() => {});
    }
    const serverId = m.serverId as number;
    if (!m.started) {
      try {
        await api(`/api/meetings/${serverId}/start`, { method: "POST", body: { speech: m.speech, transcript_only: m.speech === "api" && m.transcriptOnly } });
      } catch (err) {
        if (err instanceof ApiError && err.status === 400 && m.speech === "api") {
          // API キーが無い。録音を失わないよう、Chrome の音声認識の会議として始める（終了時に API で確定もできる）。
          await api(`/api/meetings/${serverId}/start`, { method: "POST", body: { speech: "chrome", transcript_only: false } });
          m = { ...m, speech: "chrome" };
          notify("API キーが無いため、Chrome の音声認識の会議として引き継ぎました。録音は残っています。", "warn", "net");
        } else {
          throw err;
        }
      }
      m = { ...m, started: true };
      await vault.putMeeting(m).catch(() => {});
    }
    rebindLocal(localId, serverId);
    await vault.rebindMeeting(localId, serverId).catch(() => {});
    // 端末で一時停止中・終了済みなら、サーバ側も一時停止にしておく（同時録音の上限に数えさせない）。
    if (!recordingHere(serverId)) await api(`/api/meetings/${serverId}/pause`, { method: "POST" }).catch(() => {});
    await reload();
    if (location.hash === localHash(localId)) navigate(`#/m/${serverId}`);
    notify(`「${m.title}」をサーバに引き継ぎました。録音を送っています。`, "info", "net");
    void settle();
    return serverId;
  } catch (err) {
    if (!isNetworkError(err)) {
      const message = err instanceof Error ? err.message : String(err);
      notify(`オフラインの会議をサーバに引き継げません（${message}）。録音はこの端末に残しています。`, "warn", "net");
    }
    return null;
  }
}

/** 終了を選んであったオフラインの会議を、送り終えたらサーバで終了する。 */
async function settle() {
  let all: vault.VaultMeeting[] = [];
  try {
    all = await vault.listMeetings();
  } catch {
    return;
  }
  for (const m of all) {
    if (m.finishClose == null || finished.has(m.localId)) continue;
    let serverId = m.serverId;
    if (serverId == null || !m.started) serverId = await bind(m.localId);
    if (serverId == null) continue;
    if (queuedFor(serverId) || pendingFor(serverId)) continue;
    try {
      await api(`/api/meetings/${serverId}/finish`, { method: "POST", body: { close: m.finishClose } });
    } catch (err) {
      if (isNetworkError(err)) continue;
      if (err instanceof ApiError && err.status === 400) {
        // 文字起こし用のキーが無いなど。書き起こしから議事録を作る。
        await api(`/api/meetings/${serverId}/finish`, { method: "POST", body: { close: "plain" } }).catch(() => {});
      }
    }
    finished.add(m.localId);
    await vault.deleteMeeting(m.localId).catch(() => {});
  }
  // 引き継ぎ済みで終了も済んだ（または終了を選んでいない）会議の記録は、録音中でなければ片付ける。
  for (const m of all) {
    if (m.serverId != null && m.started && m.finishClose == null && !recordingHere(m.serverId) && !queuedFor(m.serverId) && !pendingFor(m.serverId)) {
      await vault.deleteMeeting(m.localId).catch(() => {});
    }
  }
  await reload();
}

setLocalBinder(bind);
void reload();
window.addEventListener("online", () => void settle());
setInterval(() => void settle(), 15000);
