import { memo, useState } from "react";
import { api, errorMessage } from "../api/client";
import type { Hit, Mark, MeetingDetail, Note, NoteKind } from "../api/types";
import { fmt } from "../lib/format";
import { markdown } from "../lib/markdown";
import { localSeconds, recordingHere } from "../recorder/recorder";
import { openDialog } from "../state/dialog";
import { patchDetail } from "../state/meeting";
import { clearNotice, notify } from "../state/notices";

export const NOTE_LABELS: Record<NoteKind, string> = {
  decision: "決定",
  todo: "宿題",
  number: "数字・日付",
  concern: "懸念",
  term: "用語",
};

const NOTE_ORDER: NoteKind[] = ["decision", "todo", "number", "concern", "term"];

/** 「ここ重要」の印に入る区間か（印の 30 秒前から印の時刻まで）。サーバの判定と同じ。 */
export function markedIds(detail: MeetingDetail): Set<number> {
  const marks: Mark[] = detail.marks || [];
  const ids = new Set<number>();
  for (const segment of detail.segments) {
    if (marks.some((mark) => segment.start_sec <= mark.at_sec && segment.start_sec >= mark.at_sec - 30)) ids.add(segment.id);
  }
  return ids;
}

async function addMark(detail: MeetingDetail) {
  const at = recordingHere(detail.meeting.id) ? localSeconds() : detail.meeting.duration_sec;
  const result = await api<{ marks: Mark[] }>(`/api/meetings/${detail.meeting.id}/marks`, { method: "POST", body: { at_sec: at } });
  patchDetail((current) => ({ ...current, marks: result.marks }));
  notify(`${fmt(at)} 付近の発言に「重要」の印を付けました。議事録で重視します。`, "info", "mark");
}

async function showRecap(meetingId: number) {
  notify("ここまでの要点をまとめています…", "info", "recap");
  let result: { recap: string };
  try {
    result = await api<{ recap: string }>(`/api/meetings/${meetingId}/recap`, { method: "POST" });
  } finally {
    clearNotice("recap");
  }
  await openDialog({
    title: "ここまでの要点",
    body: (
      <div className="md recap" dangerouslySetInnerHTML={{ __html: markdown(result.recap) }} />
    ),
    confirm: "閉じる",
    cancel: "閉じる",
  });
}

/**
 * 会議中の聞き手の補助。次に聞くとよい問いかけ（AI）と、クリックだけで使う操作。
 * キーボードを打たずに済むよう、どれもボタン 1 つにする。
 */
export const AssistBar = memo(function AssistBar({ detail }: { detail: MeetingDetail }) {
  const [busy, setBusy] = useState(false);
  const hint = detail.meeting.next_hint;
  const upcoming = detail.questions.find((q) => q.status === "open");
  const match = hint.match(/^\[(.+?)\]\s*(.*)$/);
  const live = ["live", "paused"].includes(detail.meeting.status);
  const run = (task: () => Promise<void>) => {
    setBusy(true);
    task()
      .catch((err) => notify(errorMessage(err), "error"))
      .finally(() => setBusy(false));
  };
  return (
    <div className="assist-bar" role="region" aria-label="聞き手の補助">
      <div className="assist-hint" aria-live="polite">
        {hint ? (
          <>
            <span className="badge ai" title="話の流れから AI が考えた問いかけです">次に聞くと良い{match ? `・${match[1]}` : ""}</span>
            <span className="assist-text">{match ? match[2] : hint}</span>
          </>
        ) : upcoming ? (
          <>
            <span className="badge">次の議題</span>
            <span className="assist-text">{upcoming.body}</span>
          </>
        ) : (
          <span className="muted">話し始めると、ここに次に聞くとよい問いかけが出ます。</span>
        )}
      </div>
      {live ? (
        <div className="assist-actions">
          <button className="btn small" type="button" disabled={busy} onClick={() => run(() => addMark(detail))} title="いま話している発言に印を付け、議事録で重視させます">
            ★ ここ重要
          </button>
          <button className="btn small" type="button" disabled={busy} onClick={() => run(() => showRecap(detail.meeting.id))} title="直近10分ほどの流れを3行にまとめます">
            ここまでの要点
          </button>
        </div>
      ) : null}
    </div>
  );
});

async function removeNote(meetingId: number, note: Note) {
  const result = await api<{ notes: Note[] }>(`/api/meetings/${meetingId}/notes/${note.id}`, { method: "DELETE" });
  patchDetail((current) => ({ ...current, notes: result.notes }));
}

/** AI が発言から拾ったメモ（決定・宿題・数字・懸念・用語）。誤りはワンクリックで消せる。 */
export const NotesPanel = memo(function NotesPanel({
  meetingId,
  notes,
  onJumpTime,
  editable = true,
}: {
  meetingId: number;
  notes: Note[];
  onJumpTime?: (sec: number) => void;
  editable?: boolean;
}) {
  if (!notes.length) {
    return <div className="empty small">決定事項・宿題・数字・懸念・用語が話に出ると、AI がここに拾います。</div>;
  }
  return (
    <div className="notes">
      {NOTE_ORDER.map((kind) => {
        const items = notes.filter((note) => note.kind === kind);
        if (!items.length) return null;
        return (
          <section key={kind} className={`note-group note-${kind}`} aria-label={NOTE_LABELS[kind]}>
            <h3>{NOTE_LABELS[kind]}</h3>
            <ul>
              {items.map((note) => (
                <li key={note.id}>
                  <span>
                    {note.text}
                    {note.detail ? <span className="muted">{kind === "term" ? `：${note.detail}` : `（${note.detail}）`}</span> : null}
                    {kind === "term" ? <span className="muted small">（AI の一般知識）</span> : null}
                  </span>
                  {note.start_sec != null ? (
                    onJumpTime ? (
                      <button type="button" className="link-btn small" onClick={() => onJumpTime(note.start_sec!)} aria-label={`${fmt(note.start_sec)} の発言へ`}>
                        {fmt(note.start_sec)}
                      </button>
                    ) : (
                      <span className="muted small">{fmt(note.start_sec)}</span>
                    )
                  ) : null}
                  {editable ? (
                    <button
                      type="button"
                      className="mini"
                      aria-label={`メモ「${note.text}」を消す`}
                      title="誤りなら消す"
                      onClick={() => void removeNote(meetingId, note).catch((err) => notify(errorMessage(err), "error"))}
                    >
                      ×
                    </button>
                  ) : null}
                </li>
              ))}
            </ul>
          </section>
        );
      })}
    </div>
  );
});

/** いまの話題で自動で見つかった資料（新しいものから 3 件）。アジェンダの欄の下に常に出す。 */
export const TopicHits = memo(function TopicHits({ hits, onOpenAll }: { hits: Hit[]; onOpenAll?: () => void }) {
  const latest = [...hits].sort((a, b) => b.id - a.id).slice(0, 3);
  return (
    <section className="topic-hits" aria-label="いまの話題の資料">
      <h3>
        いまの話題の資料
        {onOpenAll && hits.length ? (
          <button type="button" className="link-btn small" onClick={onOpenAll}>
            すべて（{hits.length}）
          </button>
        ) : null}
      </h3>
      {latest.length ? (
        <ul>
          {latest.map((hit) => (
            <li key={hit.id}>
              <strong>{hit.title}</strong>
              <span className="muted small"> ・ 検索語「{hit.query}」</span>
              <p className="small clamp">{hit.snippet}</p>
            </li>
          ))}
        </ul>
      ) : (
        <p className="muted small">
          話題に合う資料が見つかると、ここに自動で出ます。社内資料は<a className="text-link" href="#/settings">設定</a>の「ローカル資料」で、顧客から受け取った資料は資料パネルの「顧客資料を添付」で登録できます。
        </p>
      )}
    </section>
  );
});
