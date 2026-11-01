import { memo, useEffect, useState } from "react";
import { api, errorMessage } from "../api/client";
import type { Evidence, MeetingDetail, Question } from "../api/types";
import { fmt } from "../lib/format";
import { localSeconds, recordingHere } from "../recorder/recorder";
import { openDialog } from "../state/dialog";
import { patchDetail } from "../state/meeting";
import { notify } from "../state/notices";

// アジェンダ（#99）。内部の名前は questions のまま、画面では「アジェンダ」「議題」と呼ぶ。
const STATUS: Record<Question["status"], string> = { open: "未着手", draft: "途中", sufficient: "完了", skipped: "今回は扱わない" };

async function patch(meetingId: number, id: number, body: Record<string, unknown>) {
  const result = await api<{ questions: Question[] }>(`/api/meetings/${meetingId}/questions/${id}`, { method: "PATCH", body });
  patchDetail((detail) => ({ ...detail, questions: result.questions }));
}

function parseMinutes(text: string | undefined): number | null {
  const value = Number((text || "").trim());
  return Number.isFinite(value) && value > 0 ? Math.round(value) : null;
}

export async function addQuestion(meetingId: number) {
  const values = await openDialog({
    title: "議題を追加",
    fields: [
      { name: "body", label: "議題（話し合うこと・確かめたいこと）" },
      { name: "minutes", label: "目安時間（分・任意）", placeholder: "例: 10" },
    ],
    confirm: "追加",
  });
  if (!values || !values.body.trim()) return;
  const result = await api<{ questions: Question[] }>(`/api/meetings/${meetingId}/questions`, {
    method: "POST",
    body: { body: values.body, minutes: parseMinutes(values.minutes) },
  });
  patchDetail((detail) => ({ ...detail, questions: result.questions }));
}

async function editQuestion(meetingId: number, question: Question) {
  const values = await openDialog({
    title: "議題を編集",
    body: <p className="muted small">結論か状態を直すと「確認済み」になり、以後は AI が上書きしません。</p>,
    fields: [
      { name: "body", label: "議題", value: question.body },
      { name: "minutes", label: "目安時間（分・空欄で目安なし）", value: question.minutes ? String(question.minutes) : "" },
      { name: "answer", label: "回答・結論", value: question.answer, type: "textarea", rows: 4 },
      {
        name: "status",
        label: "状態",
        value: question.status === "skipped" ? "open" : question.status,
        type: "select",
        options: [
          { value: "open", label: "未着手" },
          { value: "draft", label: "途中" },
          { value: "sufficient", label: "完了" },
        ],
      },
    ],
    confirm: "保存",
  });
  if (!values) return;
  const body: Record<string, unknown> = {};
  if (values.body.trim() !== question.body) body.body = values.body;
  if (values.answer.trim() !== question.answer) body.answer = values.answer;
  if (values.status !== question.status && question.status !== "skipped") body.status = values.status;
  const minutes = parseMinutes(values.minutes);
  if (minutes !== (question.minutes ?? null)) body.minutes = minutes ?? 0;
  if (Object.keys(body).length) await patch(meetingId, question.id, body);
}

function run(task: Promise<unknown>) {
  task.catch((err) => notify(errorMessage(err), "error"));
}

/** いまの録音時間（秒）。この画面で録音していればその長さ、そうでなければサーバが受け取った長さ。1 秒ごとに更新。 */
export function useLiveSeconds(detail: MeetingDetail): number {
  const id = detail.meeting.id;
  const server = detail.meeting.duration_sec || 0;
  const read = () => (recordingHere(id) ? Math.max(localSeconds(), server) : server);
  const [now, setNow] = useState(read);
  useEffect(() => {
    setNow(read());
    if (!["live", "paused"].includes(detail.meeting.status)) return;
    const timer = setInterval(() => setNow(read()), 1000);
    return () => clearInterval(timer);
  }, [id, server, detail.meeting.status]); // eslint-disable-line react-hooks/exhaustive-deps
  return now;
}

/** 議題ごとの経過秒。いまの議題は、いまの録音時間までを足す。 */
export function spentOf(question: Question, now: number): number {
  return (question.spent_sec || 0) + (question.active_since != null ? Math.max(0, now - question.active_since) : 0);
}

function minutesText(sec: number): string {
  return fmt(Math.max(0, sec));
}

/** 進行の見出し: 完了の件数、経過と目安、いまの議題の残り時間、完了して次へ。 */
export function AgendaProgress({ detail, now, live }: { detail: MeetingDetail; now: number; live: boolean }) {
  const items = detail.questions.filter((q) => q.status !== "skipped");
  if (!items.length) return null;
  const done = items.filter((q) => q.status === "sufficient").length;
  const planned = items.reduce((total, q) => total + (q.minutes || 0), 0);
  const remaining = items.filter((q) => q.status !== "sufficient").reduce((total, q) => total + (q.minutes || 0), 0);
  const active = items.find((q) => q.id === detail.meeting.active_question_id);
  const spent = active ? spentOf(active, now) : 0;
  const over = !!(active?.minutes && spent > active.minutes * 60);
  return (
    <div className="agenda-progress" aria-live="polite">
      <div className="agenda-stats">
        <span>
          完了 <strong>{done}</strong> / {items.length}
        </span>
        <progress max={items.length} value={done} aria-label="アジェンダの進み具合" />
        <span className="muted small">
          経過 {minutesText(now)}
          {planned ? ` ／ 目安の合計 ${planned}分（残り ${remaining}分）` : ""}
        </span>
      </div>
      {active ? (
        <div className={`agenda-now ${over ? "over" : ""}`}>
          <span className="badge">いまの議題</span>
          <strong className="agenda-now-title">
            {active.position + 1}. {active.body}
          </strong>
          <span className="small">
            {minutesText(spent)}
            {active.minutes ? ` ／ 目安 ${active.minutes}分${over ? "（超過）" : ""}` : ""}
          </span>
          {live ? (
            <button
              className="btn small primary"
              type="button"
              onClick={() => run(patch(detail.meeting.id, active.id, { complete_next: true, at_sec: now }))}
              title="この議題を完了にして、次の議題へ進みます"
            >
              完了して次へ
            </button>
          ) : null}
        </div>
      ) : live ? (
        <p className="muted small">いまの議題は未定です。話題から AI が判定するか、議題の「この議題へ」で始めてください。</p>
      ) : null}
    </div>
  );
}

/** 作成元と確認状態。AI の判定と人の確認を混同させない。数値の確からしさは作らない。 */
function Provenance({ question }: { question: Question }) {
  if (question.locked) return <span className="badge ok">✓ 確認済み</span>;
  if (question.answer && question.source !== "human") return <span className="badge ai">AI作成・未確認</span>;
  return null;
}

function EvidenceLinks({ evidence, answer, onJump }: { evidence: Evidence[]; answer: string; onJump: (item: Evidence) => void }) {
  if (!answer) return null;
  if (!evidence.length) return <p className="evidence muted small">根拠未確認（対応する発言を特定できていません）</p>;
  return (
    <p className="evidence small">
      根拠:{" "}
      {evidence.slice(0, 4).map((item) => (
        <button
          key={`${item.segment_id}-${item.start_sec}`}
          type="button"
          className="link-btn"
          title={item.quote}
          onClick={() => onJump(item)}
          aria-label={`根拠 ${fmt(item.start_sec)} の発言へ移動: ${item.quote}`}
        >
          {fmt(item.start_sec)}の発言
        </button>
      ))}
      {evidence.length > 4 ? <span className="muted"> ほか {evidence.length - 4} 件</span> : null}
    </p>
  );
}

const QuestionCard = memo(function QuestionCard({
  meetingId,
  question,
  active,
  live,
  spent,
  now,
  onJump,
}: {
  meetingId: number;
  question: Question;
  active: boolean;
  live: boolean;
  spent: number;
  now: number;
  onJump: (item: Evidence) => void;
}) {
  const number = question.position + 1;
  const skipped = question.status === "skipped";
  const over = !!(question.minutes && spent > question.minutes * 60);
  return (
    <article className={`q-card ${question.status} ${active ? "active" : ""}`} aria-label={`議題${number}`}>
      <h3 className="q-title">
        {number}. {question.body}
      </h3>
      <div className="q-meta">
        <span className={`badge status-${question.status}`}>{STATUS[question.status]}</span>
        {question.origin === "ai" ? <span className="badge ai" title="会議中の話の流れから AI が足しました">AI が追加</span> : null}
        <Provenance question={question} />
        {active && !skipped ? <span className="badge now">いまの議題</span> : null}
        {question.minutes || spent >= 1 ? (
          <span className={`agenda-time small ${over ? "over" : ""}`} title="この議題で話していた時間 ／ 目安">
            {spent >= 1 ? minutesText(spent) : "0:00"}
            {question.minutes ? ` ／ 目安 ${question.minutes}分` : ""}
          </span>
        ) : null}
      </div>
      {skipped && question.skipped_by === "ai" ? (
        <p className="muted small">AI が不要と判断{question.skip_reason ? `: ${question.skip_reason}` : ""}</p>
      ) : null}
      <p className="q-answer">{question.answer || <span className="muted">まだ回答・結論の候補はありません</span>}</p>
      <EvidenceLinks evidence={question.evidence || []} answer={question.answer} onJump={onJump} />
      <div className="mini-actions">
        {skipped ? (
          <button className="btn small" type="button" onClick={() => run(patch(meetingId, question.id, { skip: false }))} aria-label={`議題${number}を扱う対象に戻す`}>
            扱う対象に戻す
          </button>
        ) : null}
        {live && !skipped && !active && question.status !== "sufficient" ? (
          <button className="btn small" type="button" onClick={() => run(patch(meetingId, question.id, { activate: true, at_sec: now }))} aria-label={`議題${number}をいまの議題にする`}>
            この議題へ
          </button>
        ) : null}
        {live && active && !skipped ? (
          <button className="btn small primary" type="button" onClick={() => run(patch(meetingId, question.id, { complete_next: true, at_sec: now }))} aria-label={`議題${number}を完了にして次へ`}>
            完了して次へ
          </button>
        ) : null}
        {!skipped && !question.locked && question.answer ? (
          <button className="btn small" type="button" onClick={() => run(patch(meetingId, question.id, { confirmed: true }))} aria-label={`議題${number}の結論を確認済みにする`}>
            確認する
          </button>
        ) : null}
        {skipped || (live && active) ? null : question.status === "sufficient" ? (
          <button className="btn small" type="button" onClick={() => run(patch(meetingId, question.id, { status: "draft" }))} aria-label={`議題${number}の完了を外す`}>
            完了を外す
          </button>
        ) : (
          <button className="btn small" type="button" onClick={() => run(patch(meetingId, question.id, { status: "sufficient" }))} aria-label={`議題${number}を完了にする`}>
            完了にする
          </button>
        )}
        {!skipped && question.status !== "sufficient" ? (
          <button
            className="btn small ghost"
            type="button"
            title="この会議では扱わない議題にします。AI の更新の対象から外れます"
            onClick={() => run(patch(meetingId, question.id, { skip: true, at_sec: now }))}
            aria-label={`議題${number}を今回は扱わないにする`}
          >
            今回は扱わない
          </button>
        ) : null}
        <button className="btn small" type="button" onClick={() => run(editQuestion(meetingId, question))} aria-label={`議題${number}を編集`}>
          編集
        </button>
      </div>
    </article>
  );
});

export const QuestionList = memo(function QuestionList({
  meetingId,
  questions,
  activeId,
  onJump,
  live = false,
  now = 0,
}: {
  meetingId: number;
  questions: Question[];
  activeId: number | null;
  onJump: (item: Evidence) => void;
  /** 会議中（録音中・一時停止中）なら、進行の操作（この議題へ・完了して次へ）を出す。 */
  live?: boolean;
  now?: number;
}) {
  if (!questions.length)
    return (
      <div className="empty">
        アジェンダはまだありません。「＋ 追加」で議題を足せます。話の流れから AI が確かめるべき事項を足していきます（全文の要約は作ります）。
      </div>
    );
  // 「今回は扱わない」にしたものは下にまとめる。
  const asking = questions.filter((q) => q.status !== "skipped");
  const skipped = questions.filter((q) => q.status === "skipped");
  const card = (question: Question, active: boolean) => (
    <QuestionCard
      key={question.id}
      meetingId={meetingId}
      question={question}
      active={active}
      live={live}
      spent={spentOf(question, now)}
      now={now}
      onJump={onJump}
    />
  );
  return (
    <>
      {asking.map((question) => card(question, question.id === activeId))}
      {skipped.length ? (
        <details className="skipped-group">
          <summary>今回は扱わない（{skipped.length}）</summary>
          {skipped.map((question) => card(question, false))}
        </details>
      ) : null}
    </>
  );
});
