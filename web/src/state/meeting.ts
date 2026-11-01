import { dropCaptionsUpTo } from "../lib/captions";
import { api, limitText, toError } from "../api/client";
import type { Meeting, MeetingDetail, Segment, SseEvent } from "../api/types";
import { createStore } from "../lib/store";
import { sleep } from "../lib/format";
import { navigate } from "../lib/router";
import { clearNotice, notify } from "./notices";
import { recordingHere, stopCapturing } from "../recorder/recorder";

// 開いている会議の詳細と、その画面更新（SSE）。録音とは別に、画面を移っても購読は続ける。
export type MeetingState = { id: number | null; detail: MeetingDetail | null; error: string };

export const meetingStore = createStore<MeetingState>({ id: null, detail: null, error: "" });

type Sse = { id: number; abort: AbortController; ready: boolean; buf: SseEvent[]; attempt: number; opened: boolean };
let sse: Sse | null = null;
let generation = 0;

export function currentDetail(): MeetingDetail | null {
  return meetingStore.get().detail;
}

export function closeSse() {
  if (sse) sse.abort.abort();
  sse = null;
}

/** 会議の購読と詳細をすべて捨てる（ログアウト時）。 */
export function closeMeeting() {
  generation += 1;
  closeSse();
  meetingStore.set({ id: null, detail: null, error: "" });
}

// 先に購読してから会議の詳細を取る。取得中に届いたイベントは溜めておき、反映後に流す。
function connectSse(id: number) {
  if (sse && sse.id === id) return;
  closeSse();
  const next: Sse = { id, abort: new AbortController(), ready: false, buf: [], attempt: 0, opened: false };
  sse = next;
  void runSse(next);
}

async function runSse(current: Sse) {
  while (sse === current) {
    try {
      const response = await fetch(`/api/meetings/${current.id}/events`, { signal: current.abort.signal });
      if (response.status === 401) {
        await toError(response);
        return;
      }
      if (response.status === 404) return;
      if (!response.ok || !response.body) throw new Error(response.statusText);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buf = "";
      while (true) {
        const chunk = await reader.read();
        if (chunk.done) break;
        buf += decoder.decode(chunk.value, { stream: true });
        const parts = buf.split("\n\n");
        buf = parts.pop() || "";
        for (const part of parts) {
          const line = part.split("\n").find((row) => row.startsWith("data:"));
          if (!line) continue;
          let data: SseEvent;
          try {
            data = JSON.parse(line.slice(5).trim());
          } catch {
            continue;
          }
          if (data.type === "hello") {
            // つなぎ直したときは、切れていた間の変化を取り直す。
            if (current.opened) void refreshDetail(current.id);
            current.opened = true;
            current.attempt = 0;
            clearNotice("sse");
            continue;
          }
          if (data.type === "resync") {
            void refreshDetail(current.id);
            continue;
          }
          if (!current.ready) current.buf.push(data);
          else applyEvent(data);
        }
      }
    } catch (err) {
      if (err instanceof DOMException && err.name === "AbortError") return;
    }
    if (sse !== current) return;
    notify("画面更新の接続が切れました。自動で再接続しています。", "warn", "sse");
    current.attempt += 1;
    await sleep(Math.min(1000 * 2 ** current.attempt, 15000));
  }
}

export async function openMeeting(id: number) {
  const gen = ++generation;
  connectSse(id);
  if (meetingStore.get().id !== id) meetingStore.set({ id, detail: null, error: "" });
  try {
    const detail = await api<MeetingDetail>(`/api/meetings/${id}`);
    if (gen !== generation) return;
    meetingStore.set({ id, detail, error: "" });
    flushSse();
  } catch (err) {
    if (gen !== generation) return;
    meetingStore.set({ id, detail: null, error: err instanceof Error ? err.message : String(err) });
  }
}

function flushSse() {
  const current = sse;
  if (!current) return;
  current.ready = true;
  const pending = current.buf;
  current.buf = [];
  pending.forEach(applyEvent);
}

export async function refreshDetail(id: number) {
  try {
    const detail = await api<MeetingDetail>(`/api/meetings/${id}`);
    if (meetingStore.get().id !== id) return;
    meetingStore.set({ id, detail, error: "" });
    checkRecording(detail.meeting);
  } catch {
    // 次の再接続でもう一度取り直す
  }
}

/** 部分的な更新。変わった部分だけ新しい参照にして、書き起こしなどを作り直さない。 */
export function patchDetail(update: (detail: MeetingDetail) => MeetingDetail) {
  const state = meetingStore.get();
  if (!state.detail) return;
  meetingStore.set({ ...state, detail: update(state.detail) });
}

export function setDetail(detail: MeetingDetail) {
  const state = meetingStore.get();
  if (state.id !== detail.meeting.id) return;
  meetingStore.set({ ...state, detail });
  checkRecording(detail.meeting);
}

export function upsertSegment(segments: Segment[], segment: Segment): Segment[] {
  const index = segments.findIndex((seg) => seg.id === segment.id);
  const next = segments.slice();
  if (index >= 0) next[index] = segment;
  else next.push(segment);
  next.sort((a, b) => a.start_sec - b.start_sec || a.id - b.id);
  return next;
}

function checkRecording(meeting: Meeting) {
  if (recordingHere(meeting.id) && (meeting.finish_requested || ["done", "wrapping", "draft"].includes(meeting.status))) {
    stopCapturing("この会議は別の画面で終了されたため、録音を止めました。");
  }
}

function applyEvent(event: SseEvent) {
  const state = meetingStore.get();
  const detail = state.detail;
  if (!detail || !sse || sse.id !== detail.meeting.id) return;
  let next: MeetingDetail = detail;
  switch (event.type) {
    case "segment":
      next = { ...detail, segments: upsertSegment(detail.segments, event.segment) };
      // AI の書き起こしが届いた区間の速報字幕は、置き換わったので消す（#104）。
      dropCaptionsUpTo(event.segment.start_sec);
      clearNotice("job");
      break;
    case "questions":
      next = {
        ...detail,
        questions: event.questions,
        meeting: { ...detail.meeting, active_question_id: event.active_question_id },
      };
      break;
    case "knowledge":
      next = { ...detail, hits: event.hits };
      break;
    case "notes":
      next = { ...detail, notes: event.notes };
      break;
    case "marks":
      next = { ...detail, marks: event.marks };
      break;
    case "hint":
      next = { ...detail, meeting: { ...detail.meeting, next_hint: event.next_hint } };
      break;
    case "jobs":
      next = {
        ...detail,
        errors: event.errors ?? detail.errors,
        meeting: {
          ...detail.meeting,
          pending_jobs: event.pending,
          retry_waiting: event.retry_waiting ?? detail.meeting.retry_waiting,
          next_retry_at: event.next_retry_at !== undefined ? event.next_retry_at : detail.meeting.next_retry_at,
          failed_jobs: event.failed ?? detail.meeting.failed_jobs,
        },
      };
      break;
    case "warning":
      notify(event.message, "warn", event.message.startsWith("文字起こしに失敗") ? "job" : "");
      return;
    case "limit":
      notify(limitText(event.limit), "warn", "limit");
      return;
    case "meeting":
      next = { ...detail, meeting: event.meeting };
      break;
    case "summarizing":
      next = { ...detail, summarizing: event.running };
      break;
    case "status":
      next = {
        ...detail,
        meeting: event.meeting ?? (event.status ? { ...detail.meeting, status: event.status } : detail.meeting),
        summarizing: event.status === "done" ? false : detail.summarizing,
      };
      break;
    case "deleted":
      if (recordingHere(detail.meeting.id)) stopCapturing("録音中の会議が削除されたため、録音を止めました。");
      notify("この会議は削除されました。", "warn");
      closeMeeting();
      if (location.hash === `#/m/${detail.meeting.id}`) navigate("#/");
      return;
    default:
      return;
  }
  meetingStore.set({ ...state, detail: next });
  checkRecording(next.meeting);
}
