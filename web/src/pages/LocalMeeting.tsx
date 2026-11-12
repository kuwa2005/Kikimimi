import { useEffect, useState } from "react";
import { errorMessage } from "../api/client";
import { configureCaptions, chromeCaptionMode, setCaptionPersist } from "../lib/captions";
import { useStore } from "../lib/store";
import { recorderState, recordingHere, saveCaption } from "../recorder/recorder";
import * as vault from "../recorder/vault";
import { exportLocalAudio } from "../recorder/export";
import { guarded, useBusy } from "../state/busy";
import { openDialog } from "../state/dialog";
import { notify } from "../state/notices";
import { finishLocal, localMeetings, pauseLocal, resumeLocal } from "../state/offline";
import { session } from "../state/session";
import { Clock, Meter } from "../components/Meter";
import { EnhanceToggle } from "../components/Enhance";
import { LiveCaptions, OrderToggle, newestFirst } from "../components/Transcript";
import { Notices } from "../components/Notices";

/** オフラインで始めた会議の画面。録音・速報字幕・一時停止・終了だけを扱う。つながると通常の会議画面へ移る。 */
export function LocalMeetingPage({ localId }: { localId: number }) {
  const rec = useStore(recorderState);
  const auth = useStore(session);
  const list = useStore(localMeetings);
  const busyName = useBusy();
  const reverse = useStore(newestFirst);
  const [meeting, setMeeting] = useState<vault.VaultMeeting | null | undefined>(undefined);
  const here = recordingHere(localId);

  useEffect(() => {
    let alive = true;
    void vault
      .getMeeting(localId)
      .catch(() => undefined)
      .then((m) => {
        if (!alive) return;
        // サーバへ引き継ぎ済みなら、通常の会議画面へ。
        if (m?.serverId != null && m.started) location.hash = `#/m/${m.serverId}`;
        else setMeeting(m ?? null);
      });
    return () => {
      alive = false;
    };
  }, [localId, list]);

  // 速報字幕（端末内の音声認識）。確定文は端末に保存し、つながったら会議の書き起こし・控えとして送る。
  useEffect(() => {
    let alive = true;
    setCaptionPersist((line) => saveCaption(localId, line.chunkStart, line.text));
    void chromeCaptionMode().then((mode) => {
      if (alive) configureCaptions(mode);
    });
    return () => {
      alive = false;
      setCaptionPersist(null);
      configureCaptions("off");
    };
  }, [localId]);

  if (meeting === undefined) return <p className="muted splash">読み込み中</p>;
  const title = meeting?.title || "オフラインの会議";
  const ended = meeting?.finishClose != null;
  const waiting = rec.queueLength + rec.parked + rec.captionsPending;
  const disabled = !!busyName;

  const finish = () =>
    guarded("finish", async () => {
      const ok = await openDialog({
        title: "会議を終了しますか？",
        body: (
          <>
            <p>録音を止めます。録音はこの端末に保存済みです。</p>
            <p className="muted">つながったら録音と書き起こしをサーバへ送り、議事録を作ります。</p>
          </>
        ),
        confirm: "終了する",
        danger: true,
      });
      if (ok) await finishLocal(localId);
      setMeeting((await vault.getMeeting(localId).catch(() => undefined)) ?? null);
    });

  return (
    <div className="page live-page">
      <header className="meeting-head">
        <div className="head-main">
          <div className="title-line">
            <h1>{title}</h1>
          </div>
          <div className="meter-line">
            <span className="offline-badge">{auth.offline ? "オフライン" : "サーバへ引き継ぎ中"}</span>
            <span className={`chip ${here ? "live" : ended ? "done" : "paused"}`}>
              {here ? <span className="dot" aria-hidden="true" /> : null}
              {here ? "録音中（この端末に保存）" : ended ? "終了（送信待ち）" : "一時停止中"}
            </span>
            <Clock meetingId={localId} fallback={meeting?.seconds || 0} />
            <Meter active={here} label="録音中の音量" />
            <EnhanceToggle />
          </div>
          <p className="muted small" aria-live="polite">
            {auth.offline
              ? "サーバにつながっていません。録音と書き起こしはこの端末に保存しています。つながると自動で会議を作って送り、そのまま録音を続けます。"
              : "サーバに会議を作っています。録音は止めていません。"}
            {waiting ? ` 未送信 ${waiting} 件。` : ""}
            {rec.vaultOk ? "" : " ※この端末に保存できていません。ブラウザを閉じないでください。"}
          </p>
        </div>
        <div className="actions head-actions">
          <button className="btn" type="button" disabled={disabled} onClick={() => void guarded("export", () => exportLocalAudio(localId, title))} title="この端末に保存している録音を WAV で書き出します">
            録音を書き出す
          </button>
          {!ended && here ? (
            <button className="btn" type="button" disabled={disabled} onClick={() => void guarded("pause", () => pauseLocal(localId))}>
              一時停止
            </button>
          ) : null}
          {!ended && !here ? (
            <button
              className="btn primary"
              type="button"
              disabled={disabled}
              onClick={() => void guarded("resume", () => resumeLocal(localId)).catch((err) => notify(errorMessage(err), "error"))}
            >
              再開
            </button>
          ) : null}
          {!ended ? (
            <button className="btn danger" type="button" disabled={disabled} onClick={() => void finish()}>
              終了
            </button>
          ) : null}
        </div>
      </header>
      <Notices />
      <section className="pane">
        <div className="body transcript" aria-label="速報字幕">
          <OrderToggle />
          {meeting?.questions.length ? (
            <details className="muted small">
              <summary>アジェンダ（{meeting.questions.length} 件）</summary>
              <ol>
                {meeting.questions.map((q, i) => (
                  <li key={i}>{q}</li>
                ))}
              </ol>
            </details>
          ) : null}
          <LiveCaptions after={-1} reverse={reverse} />
          <p className="muted small">回答候補・ナレッジ検索・議事録は、サーバにつながってから動きます。</p>
        </div>
      </section>
    </div>
  );
}
