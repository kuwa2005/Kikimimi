import { useEffect, useRef, useState } from "react";
import { api, errorMessage } from "../api/client";
import type { MeetingDetail } from "../api/types";
import { fmt } from "../lib/format";
import { AUDIO_ACCEPT, decodeAudioFile, parseTranscript, splitPieces, TEXT_ACCEPT, uploadPieces, type Piece, type TextLine } from "../lib/importer";
import { refreshDetail } from "../state/meeting";
import { notify } from "../state/notices";

type Loaded =
  | { kind: "audio"; name: string; pieces: Piece[]; seconds: number; recordedStart: Date | null }
  | { kind: "text"; name: string; lines: TextLine[]; timed: boolean };

function isText(file: File): boolean {
  return /\.(txt|vtt|srt)$/i.test(file.name) || file.type.startsWith("text/");
}

/** datetime-local の値（地域時刻）。 */
function localInput(date: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/**
 * ボイスレコーダーの音声・文字起こし済みテキストを、この会議に取り込む（#96）。
 * 置く位置は「続きとして末尾に足す」か「開始時刻を指定（録音ファイルの日時から自動で入れる）」。
 */
export function ImportDialog({ detail, onClose }: { detail: MeetingDetail; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [reading, setReading] = useState(false);
  const [place, setPlace] = useState<"end" | "time">("end");
  const [startAt, setStartAt] = useState("");
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [error, setError] = useState("");
  const abort = useRef<AbortController | null>(null);
  const meeting = detail.meeting;
  const meetingStart = meeting.started_at ? new Date(meeting.started_at) : null;

  useEffect(() => {
    ref.current?.showModal();
    return () => abort.current?.abort();
  }, []);

  const pick = async (file: File | undefined) => {
    setError("");
    setLoaded(null);
    if (!file) return;
    if (file.size > 500 * 1024 * 1024) {
      setError("ファイルが大きすぎます（500MB まで）");
      return;
    }
    if (/\.doc$/i.test(file.name)) {
      setError("Word 97-2003 形式（.doc）は読めません。資料は「顧客資料」から添付してください（docx か PDF で保存し直してください）");
      return;
    }
    setReading(true);
    try {
      if (isText(file)) {
        const { lines, timed } = parseTranscript(await file.text());
        if (!lines.length) throw new Error("取り込める文が見つかりません");
        setLoaded({ kind: "text", name: file.name, lines, timed });
        setPlace("end");
      } else {
        const samples = await decodeAudioFile(file);
        const seconds = samples.length / 16000;
        // 多くのレコーダーは、録音を終えた時刻をファイルの日時に残す。そこから録音の開始時刻を推定する。
        const recordedStart = file.lastModified ? new Date(file.lastModified - seconds * 1000) : null;
        setLoaded({ kind: "audio", name: file.name, pieces: splitPieces(samples), seconds, recordedStart });
        if (recordedStart && meetingStart && recordedStart.getTime() >= meetingStart.getTime()) {
          setPlace("time");
          setStartAt(localInput(recordedStart));
        } else {
          setPlace("end");
          if (recordedStart) setStartAt(localInput(recordedStart));
        }
      }
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setReading(false);
    }
  };

  /** 会議の録音時間の上での開始秒。 */
  const baseSec = (): number => {
    if (place === "end") return Math.ceil(meeting.duration_sec || 0) + 1;
    if (!meetingStart) throw new Error("会議の開始時刻が無いため、時刻では置けません。「末尾に足す」を選んでください");
    const at = new Date(startAt);
    if (Number.isNaN(at.getTime())) throw new Error("開始時刻を入れてください");
    const sec = (at.getTime() - meetingStart.getTime()) / 1000;
    if (sec < 0) throw new Error("会議の開始より前には置けません。「末尾に足す」を選ぶか、時刻を直してください");
    return Math.round(sec);
  };

  const run = async () => {
    if (!loaded) return;
    setError("");
    try {
      const base = baseSec();
      if (loaded.kind === "audio") {
        abort.current = new AbortController();
        setProgress({ done: 0, total: loaded.pieces.length });
        await uploadPieces(meeting.id, base, loaded.pieces, (done) => setProgress({ done, total: loaded.pieces.length }), abort.current.signal);
        notify(`「${loaded.name}」（${fmt(loaded.seconds)}）を取り込みました。文字起こしと回答候補を順に作ります。${meeting.status === "done" ? "終わると要約を作り直します。" : ""}`, "info");
      } else {
        const segments = loaded.lines.map((line) => ({ start_sec: base + line.start_sec, text: line.text }));
        const result = await api<{ imported: number; skipped: number }>(`/api/meetings/${meeting.id}/import-text`, { method: "POST", body: { segments } });
        notify(
          `「${loaded.name}」から ${result.imported} 区間を取り込みました${result.skipped ? `（同じ時刻の ${result.skipped} 区間は除外）` : ""}。回答候補とメモを作っています。`,
          "info",
        );
      }
      await refreshDetail(meeting.id);
      onClose();
    } catch (err) {
      setError(errorMessage(err));
      setProgress(null);
      await refreshDetail(meeting.id).catch(() => {});
    }
  };

  const busy = reading || progress != null;
  return (
    <dialog
      ref={ref}
      className="confirm wide"
      aria-labelledby="import-title"
      onCancel={(event) => {
        event.preventDefault();
        if (!progress) onClose();
      }}
    >
      <h2 className="dlg-title" id="import-title">
        録音・書き起こしを取り込む
      </h2>
      <p className="muted small">
        ボイスレコーダーの音声（mp3・m4a・wav など）や、文字起こし済みのテキスト（txt・vtt・srt）を、この会議に足します。PC から離れて聞いた話も、1 本の議事録につながります。
      </p>
      <div className="field">
        <label htmlFor="import-file">ファイル</label>
        <input id="import-file" type="file" accept={`${AUDIO_ACCEPT},${TEXT_ACCEPT}`} disabled={busy} onChange={(event) => void pick(event.target.files?.[0])} />
      </div>
      {reading ? <p className="muted">読み込んでいます…</p> : null}
      {loaded ? (
        <>
          <p className="note">
            {loaded.kind === "audio"
              ? `音声 ${fmt(loaded.seconds)}（${loaded.pieces.length} 区間に分けて送ります）`
              : `テキスト ${loaded.lines.length} 区間${loaded.timed ? "（時刻つき）" : "（時刻なし: 段落ごとに並べます）"}`}
          </p>
          <fieldset className="field">
            <legend>置く位置</legend>
            <label className="check">
              <input type="radio" name="import-place" checked={place === "end"} onChange={() => setPlace("end")} disabled={busy} />
              続きとして末尾に足す（いまの録音 {fmt(meeting.duration_sec || 0)} のあと）
            </label>
            <label className="check">
              <input type="radio" name="import-place" checked={place === "time"} onChange={() => setPlace("time")} disabled={busy || !meetingStart} />
              開始時刻に合わせて差し込む
            </label>
            {place === "time" ? (
              <div className="field">
                <label htmlFor="import-start">この{loaded.kind === "audio" ? "録音" : "書き起こし"}の開始時刻</label>
                <input id="import-start" type="datetime-local" step={1} value={startAt} onChange={(event) => setStartAt(event.target.value)} disabled={busy} />
                <p className="hint">
                  {loaded.kind === "audio" && loaded.recordedStart ? "ファイルの日時から推定した時刻を入れています。" : ""}
                  会議の開始（{meetingStart ? meetingStart.toLocaleString() : "未開始"}）より後で、既存の録音と重ならない時刻にしてください。
                </p>
              </div>
            ) : null}
          </fieldset>
        </>
      ) : null}
      {progress ? (
        <p aria-live="polite">
          送信中… {progress.done} / {progress.total}
          <progress max={progress.total} value={progress.done} />
        </p>
      ) : null}
      {error ? <div className="banner error">{error}</div> : null}
      <div className="actions end">
        {progress ? (
          <button className="btn" type="button" onClick={() => abort.current?.abort()}>
            中止
          </button>
        ) : (
          <button className="btn" type="button" onClick={onClose}>
            閉じる
          </button>
        )}
        <button className="btn primary" type="button" disabled={!loaded || busy} onClick={() => void run()}>
          取り込む
        </button>
      </div>
    </dialog>
  );
}
