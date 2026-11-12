import { api } from "../api/client";
import type { MeetingDetail, Settings } from "../api/types";
import { fmt, fmtLocal } from "../lib/format";
import { navigate } from "../lib/router";
import { useStore } from "../lib/store";
import { queuedFor, recorderState, recordingHere } from "../recorder/recorder";
import { finishMeeting, pauseMeeting, resumeMeeting, transcribeReady } from "../state/actions";
import { guarded, useBusy } from "../state/busy";
import { openDialog } from "../state/dialog";
import { refreshDetail, setDetail } from "../state/meeting";
import { notify } from "../state/notices";
import { captions } from "../lib/captions";
import { Clock, Meter } from "./Meter";
import { EnhanceToggle } from "./Enhance";
import type { ReactNode } from "react";

function stage(detail: MeetingDetail, here: boolean) {
  const meeting = detail.meeting;
  if (meeting.status === "done") return { chip: "done", label: "完了" };
  if (meeting.status === "wrapping") return { chip: "wrapping", label: "要約を作成中" };
  if (meeting.finish_requested) return { chip: "wrapping", label: "終了処理中" };
  if (meeting.status === "paused") return { chip: "paused", label: "一時停止中" };
  if (here) return { chip: "live", label: "録音中" };
  return { chip: "idle", label: "このタブでは録音していません" };
}

function lagText(detail: MeetingDetail, here: boolean): string {
  const meeting = detail.meeting;
  const local = queuedFor(meeting.id);
  const parts: string[] = [];
  if (local + meeting.pending_jobs) parts.push(`文字起こし待ち ${local + meeting.pending_jobs} 区間`);
  if (meeting.retry_waiting) {
    const at = meeting.next_retry_at ? `（${fmtLocal(meeting.next_retry_at)} に再試行）` : "";
    parts.push(`再試行待ち ${meeting.retry_waiting} 区間${at}`);
  }
  if (meeting.failed_jobs) parts.push(`失敗 ${meeting.failed_jobs} 区間`);
  if (!parts.length) return here ? "文字起こしは追いついています" : "";
  return `${parts.join(" / ")}。${here ? "録音は止まっていません" : "録音は止まっています"}`;
}

async function editInfo(detail: MeetingDetail) {
  const meeting = detail.meeting;
  const values = await openDialog({
    title: "会議の情報を編集",
    fields: [
      { name: "title", label: "タイトル", value: meeting.title },
      { name: "participants", label: "参加者", value: meeting.participants || "", placeholder: "例: 田中、佐藤、鈴木" },
    ],
    confirm: "保存",
  });
  if (!values) return;
  setDetail(await api<MeetingDetail>(`/api/meetings/${meeting.id}`, { method: "PATCH", body: { title: values.title, participants: values.participants } }));
}

function SpeechLive({ detail }: { detail: MeetingDetail }) {
  const meeting = detail.meeting;
  const busyName = useBusy();
  const live = (meeting.status === "live" || meeting.status === "paused") && !meeting.finish_requested;
  if (!live) return null;
  const speech = meeting.speech || "api";
  const apply = (next: "api" | "chrome" | "off", transcriptOnly = !!meeting.transcript_only) =>
    guarded("speech", async () => {
      if (next === "api") {
        const settings = await api<Settings>("/api/settings");
        if (!transcribeReady(settings)) {
          notify("API の文字起こしには、文字起こし用の APIキーが必要です。", "warn");
          navigate("#/settings");
          return;
        }
      }
      setDetail(
        await api<MeetingDetail>(`/api/meetings/${meeting.id}/speech`, {
          method: "POST",
          body: { speech: next, transcript_only: next === "api" && transcriptOnly },
        }),
      );
    });
  return (
    <fieldset className="speech-live">
      <legend>録音中の文字起こし</legend>
      <label className="check">
        <input type="radio" name={`speech-${meeting.id}`} checked={speech === "api"} disabled={!!busyName} onChange={() => void apply("api")} />
        APIで文字起こし
      </label>
      <label className="check">
        <input type="radio" name={`speech-${meeting.id}`} checked={speech === "chrome"} disabled={!!busyName} onChange={() => void apply("chrome")} />
        Chromeのみ（APIキー不要）
      </label>
      <label className="check">
        <input type="radio" name={`speech-${meeting.id}`} checked={speech === "off"} disabled={!!busyName} onChange={() => void apply("off")} />
        録音だけ
      </label>
      {speech === "api" ? (
        <label className="check">
          <input
            type="checkbox"
            checked={!!meeting.transcript_only}
            disabled={!!busyName}
            onChange={(event) => void apply("api", event.target.checked)}
          />
          文字起こしのみ
        </label>
      ) : null}
    </fieldset>
  );
}

/** 会議の見出し。状態のチップと、状態に合う操作だけを出す。 */
export function MeetingHead({ detail, extra }: { detail: MeetingDetail; extra?: ReactNode }) {
  const rec = useStore(recorderState);
  const busyName = useBusy();
  const meeting = detail.meeting;
  const here = recordingHere(meeting.id);
  const current = stage(detail, here);
  const live = meeting.status !== "done";
  const retryable = meeting.failed_jobs + meeting.retry_waiting;
  const disabled = !!busyName;
  const meta = [
    meeting.started_at ? fmtLocal(meeting.started_at) : "",
    !live ? `録音 ${fmt(meeting.duration_sec)}` : "",
    meeting.participants ? `参加者${meeting.participants_auto ? "（発言から自動）" : ""}: ${meeting.participants}` : "",
  ].filter(Boolean);
  const id = meeting.id;
  const speech = meeting.speech || "api";
  const captionProblem = useStore(captions).problem;
  const ears = (meeting.status === "live" || meeting.status === "paused") && !meeting.finish_requested;

  const buttons: ReactNode[] = [];
  if (meeting.finish_requested && meeting.status !== "wrapping" && meeting.status !== "done") {
    buttons.push(
      <button key="cancel" className="btn" type="button" disabled={disabled} onClick={() => void guarded("cancel", async () => {
        setDetail(await api<MeetingDetail>(`/api/meetings/${id}/cancel-finish`, { method: "POST" }));
        notify("終了を取り消しました。「再開」で録音を続けられます。", "info");
      })}>
        終了を取り消して録音に戻る
      </button>,
    );
    if (meeting.retry_waiting) {
      buttons.push(
        <button key="giveup" className="btn" type="button" disabled={disabled} onClick={() => void guarded("giveup", async () => {
          await api(`/api/meetings/${id}/give-up-retries`, { method: "POST" });
          notify("再試行待ちの区間を欠落として、要約に進みます。", "info");
          await refreshDetail(id);
        })}>
          再試行をあきらめて要約する
        </button>,
      );
    }
  } else if (live && meeting.status !== "wrapping") {
    if (here) {
      buttons.push(
        <button key="pause" className="btn" type="button" disabled={disabled} onClick={() => void guarded("pause", () => pauseMeeting(id))}>
          一時停止
        </button>,
      );
    } else {
      buttons.push(
        <button key="resume" className="btn primary" type="button" disabled={disabled} onClick={() => void guarded("resume", () => resumeMeeting(id))}>
          {meeting.status === "paused" ? "再開" : "このタブで録音を再開"}
        </button>,
      );
    }
    buttons.push(
      <button key="finish" className="btn danger" type="button" disabled={disabled} onClick={() => void guarded("finish", () => finishMeeting(id))}>
        終了
      </button>,
    );
  }
  if (retryable && meeting.status !== "wrapping") {
    buttons.push(
      <button key="retry" className="btn" type="button" disabled={disabled} onClick={() => void guarded("retry", async () => {
        await api(`/api/meetings/${id}/retry-audio`, { method: "POST" });
        notify("失敗した区間を再試行します。", "info");
        await refreshDetail(id);
      })}>
        {meeting.retry_waiting ? "今すぐ再試行" : "失敗した文字起こしを再試行"}
      </button>,
    );
  }

  return (
    <header className="meeting-head">
      <div className="head-main">
        <div className="title-line">
          <h1>{meeting.title}</h1>
          <button className="btn small ghost" type="button" onClick={() => void guarded("info", () => editInfo(detail))} aria-label="タイトルと参加者を編集">
            ✎ 編集
          </button>
        </div>
        <div className="meter-line">
          <span className={`chip ${current.chip}`}>
            {current.chip === "live" ? <span className="dot" aria-hidden="true" /> : null}
            {current.label}
          </span>
          {live ? (
            <>
              <Clock meetingId={id} fallback={meeting.duration_sec} />
              <Meter active={here} label="録音中の音量" />
              {ears ? <EnhanceToggle /> : null}
            </>
          ) : null}
          {meta.length ? <span className="muted">{meta.join(" ・ ")}</span> : null}
        </div>
        <SpeechLive detail={detail} />
        <p className="muted small lag" aria-live="polite">
          {ears && speech === "chrome"
            ? "Chrome の音声認識で書き起こしています。APIキーは使っていません。"
            : ears && speech === "api" && here
              ? `Chrome の速報を出しています。${lagText(detail, here)}`
              : ears && speech === "off"
                ? "録音だけ残しています。文字起こしは終了時に選べます。"
                : lagText(detail, here)}
        </p>
        {ears && speech !== "off" && captionProblem ? <p className="muted small">{captionProblem}</p> : null}
        {here && rec.deviceLabel ? (
          <p className="muted small">
            マイク: {rec.deviceLabel}
            {rec.tabShared ? " ＋ 共有タブの音声" : ""}
          </p>
        ) : null}
      </div>
      <div className="actions head-actions">
        {extra}
        {buttons}
      </div>
    </header>
  );
}
