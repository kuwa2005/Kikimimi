import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorMessage, request } from "../api/client";
import type { Evidence, ExportScope, MeetingDetail, Segment } from "../api/types";
import { fmt } from "../lib/format";
import { useMediaQuery } from "../lib/media";
import { pref, PREF_PANEL, savePref } from "../lib/prefs";
import { useStore } from "../lib/store";
import { localSeconds, recordingHere } from "../recorder/recorder";
import { copyText, exportMarkdown } from "../state/actions";
import { guarded, useBusy } from "../state/busy";
import { openDialog } from "../state/dialog";
import { meetingStore, openMeeting, patchDetail, setDetail, upsertSegment } from "../state/meeting";
import { notify } from "../state/notices";
import { markdown } from "../lib/markdown";
import { MeetingHead } from "../components/MeetingHead";
import { Notices } from "../components/Notices";
import { Transcript } from "../components/Transcript";
import { addQuestion, AgendaProgress, QuestionList, useLiveSeconds } from "../components/Questions";
import { KnowledgePanel } from "../components/Knowledge";
import { TabPanel, Tabs } from "../components/Tabs";
import { AssistBar, markedIds, NotesPanel, TopicHits } from "../components/Assist";
import { ImportDialog } from "../components/Import";
import { chromeCaptionMode, configureCaptions, setCaptionPersist, setCaptionPhrases } from "../lib/captions";
import { sessionGet, sessionRemove } from "../lib/prefs";

/** 新しい会議の画面から「ファイルから取り込む」で来たら、取り込みの画面を開く。 */
function useImportDialog(meetingId: number) {
  const [open, setOpen] = useState(() => sessionGet<number>("kikimimi.openImport") === meetingId);
  useEffect(() => {
    if (open) sessionRemove("kikimimi.openImport");
  }, [open]);
  return [open, setOpen] as const;
}
import { MeetingForm } from "./MeetingForm";

async function editSegment(detail: MeetingDetail, segment: Segment) {
  const values = await openDialog({
    title: `書き起こしを直す（${fmt(segment.start_sec)}）`,
    body: detail.meeting.status === "done" ? <p className="muted small">直した内容を要約に反映するには「要約を作り直す」を押してください。</p> : undefined,
    fields: [
      { name: "speaker", label: "話者（空にすると、会議の終了後の推定に任せます）", value: segment.speaker || "", placeholder: "例: 田中" },
      { name: "text", label: "発言", value: segment.text, type: "textarea", rows: 5 },
    ],
    confirm: "保存",
  });
  if (!values) return;
  const result = await api<{ segment: Segment }>(`/api/meetings/${detail.meeting.id}/segments/${segment.id}`, {
    method: "PATCH",
    body: { text: values.text, ...(values.speaker.trim() !== (segment.speaker || "") ? { speaker: values.speaker } : {}) },
  });
  patchDetail((current) => ({ ...current, segments: upsertSegment(current.segments, result.segment) }));
}

export function MeetingPage({ id }: { id: number }) {
  const state = useStore(meetingStore);
  useEffect(() => {
    void openMeeting(id);
  }, [id]);
  const detail = state.id === id ? state.detail : null;
  if (!detail) {
    return (
      <div className="page">
        <Notices />
        {state.id === id && state.error ? <div className="banner error">{state.error}</div> : <p className="muted">読み込み中</p>}
      </div>
    );
  }
  if (detail.meeting.status === "draft") return <MeetingForm key={`draft-${id}`} detail={detail} />;
  if (detail.meeting.status === "done") return <DoneView key={`done-${id}`} detail={detail} />;
  return <LiveView key={`live-${id}`} detail={detail} />;
}

/** 録音時間の上限が近づいたら一度だけ知らせる。 */
function useLimitWarning(detail: MeetingDetail) {
  const warned = useRef(false);
  const limit = detail.limits?.minutes_limit ?? null;
  const used = detail.limits?.minutes_used ?? 0;
  const id = detail.meeting.id;
  useEffect(() => {
    if (limit == null) return;
    const timer = setInterval(() => {
      const seconds = Math.max(used * 60, recordingHere(id) ? localSeconds() : 0);
      const left = limit * 60 - seconds;
      if (left < 300 && !warned.current) {
        warned.current = true;
        notify(`この会議の録音時間の上限（${limit} 分）まで、残り ${Math.max(0, Math.ceil(left / 60))} 分です。上限に達すると録音を止めます。`, "warn", "limit");
      }
    }, 2000);
    return () => clearInterval(timer);
  }, [limit, used, id]);
}

function SegSearch({ id, value, onChange }: { id: string; value: string; onChange: (value: string) => void }) {
  return (
    <>
      <label htmlFor={id} className="sr-only">
        書き起こしを検索
      </label>
      <input id={id} type="search" className="compact" placeholder="書き起こしを検索" value={value} onChange={(event) => onChange(event.target.value)} />
    </>
  );
}

function LiveView({ detail }: { detail: MeetingDetail }) {
  const wide = useMediaQuery("(min-width: 1380px)");
  const mid = useMediaQuery("(min-width: 960px)");
  const [panelOpen, setPanelOpen] = useState(() => pref(PREF_PANEL, "1") === "1");
  const [query, setQuery] = useState("");
  const [tab, setTab] = useState("transcript");
  const [flashId, setFlashId] = useState<number | null>(null);
  const [seenHits, setSeenHits] = useState(() => detail.hits.length);
  const [importing, setImporting] = useImportDialog(detail.meeting.id);
  const meetingId = detail.meeting.id;
  const now = useLiveSeconds(detail);
  // 速報字幕（#104）: 設定とプランで ON のときだけ。議題と参加者名を、当てやすくする語として渡す。
  const agendaWords = detail.questions.map((q) => q.body).join("\n") + "\n" + (detail.meeting.participants || "");
  useEffect(() => {
    setCaptionPhrases(agendaWords.split(/[\n、,，\s]+/).filter((word) => word.length >= 2 && word.length <= 20));
  }, [agendaWords]);
  const speech = detail.meeting.speech || "api";
  useEffect(() => {
    if (speech === "off") {
      setCaptionPersist(null);
      configureCaptions("off");
      return () => {
        setCaptionPersist(null);
        configureCaptions("off");
      };
    }
    // api: Chrome の速報 ＋ API の文字起こし。chrome: Chrome の音声認識だけを保存する。
    let alive = true;
    setCaptionPersist(
      speech === "chrome"
        ? (line) => {
            void api(`/api/meetings/${meetingId}/captions`, { method: "POST", body: { start_sec: line.chunkStart, text: line.text } }).catch((err) =>
              notify(errorMessage(err), "error", "caption"),
            );
          }
        : null,
    );
    void chromeCaptionMode().then((captionMode) => {
      if (!alive) return;
      configureCaptions(captionMode);
      if (captionMode === "off") {
        notify("このブラウザは Chrome の音声認識に対応していません。デスクトップの Chrome で開いてください。", "warn", "caption");
      }
    });
    return () => {
      alive = false;
      setCaptionPersist(null);
      configureCaptions("off");
    };
  }, [meetingId, speech]);
  useLimitWarning(detail);
  const marked = markedIds(detail);
  const notes = detail.notes || [];
  const showingKnowledge = (wide && panelOpen) || tab === "knowledge";
  useEffect(() => {
    if (showingKnowledge) setSeenHits(detail.hits.length);
  }, [showingKnowledge, detail.hits.length]);
  const newHits = Math.max(0, detail.hits.length - seenHits);
  const knowledgeLabel = newHits ? `関連ナレッジ（新着 ${newHits}）` : "関連ナレッジ";
  const jumpTime = useCallback(
    (sec: number) => {
      const segment = meetingStore.get().detail?.segments.find((item) => item.start_sec === sec);
      if (!segment) return;
      setQuery("");
      if (!mid) setTab("transcript");
      setFlashId(null);
      setTimeout(() => setFlashId(segment.id), 0);
    },
    [mid],
  );

  const onEdit = useCallback((segment: Segment) => void editSegment(meetingStore.get().detail!, segment).catch((err) => notify(errorMessage(err), "error")), []);
  const onSeek = useCallback(() => {}, []);
  const onJump = useCallback(
    (item: Evidence) => {
      setQuery("");
      if (!mid) setTab("transcript");
      setFlashId(null);
      setTimeout(() => setFlashId(item.segment_id), 0);
    },
    [mid],
  );

  const transcript = (
    <Transcript segments={detail.segments} query={query} seekable={false} playingId={null} flashId={flashId} onSeek={onSeek} onEdit={onEdit} follow marked={marked} live />
  );
  const openKnowledge = () => {
    if (wide) {
      setPanelOpen(true);
      savePref(PREF_PANEL, "1");
    } else setTab("knowledge");
  };
  // アジェンダの下に「いまの話題の資料」を出す（資料パネルを開いているときは出さない）。
  const questions = (
    <>
      <AgendaProgress detail={detail} now={now} live />
      <QuestionList meetingId={meetingId} questions={detail.questions} activeId={detail.meeting.active_question_id} onJump={onJump} live now={now} />
      {wide && panelOpen ? null : <TopicHits hits={detail.hits} onOpenAll={openKnowledge} />}
    </>
  );
  const memo = <NotesPanel meetingId={meetingId} notes={notes} onJumpTime={jumpTime} />;
  const addButton = (
    <button className="btn small" type="button" onClick={() => void addQuestion(meetingId).catch((err) => notify(errorMessage(err), "error"))}>
      ＋ 追加
    </button>
  );
  const knowledge = <KnowledgePanel detail={detail} done={false} />;
  const togglePanel = () => {
    setPanelOpen(!panelOpen);
    savePref(PREF_PANEL, panelOpen ? "0" : "1");
  };

  let body;
  if (wide) {
    body = (
      <div className={`live-grid ${panelOpen ? "with-panel" : ""}`}>
        <section className="pane" aria-labelledby="h-transcript">
          <div className="pane-head">
            <h2 id="h-transcript">書き起こし</h2>
            <SegSearch id="seg-search" value={query} onChange={setQuery} />
          </div>
          {transcript}
        </section>
        <section className="pane" aria-labelledby="h-questions">
          <div className="pane-head">
            <h2 id="h-questions">アジェンダ</h2>
            {addButton}
          </div>
          <div className="body">
            {questions}
            <section className="memo-block" aria-labelledby="h-memo">
              <h2 id="h-memo">メモ（AI が発言から拾ったもの）</h2>
              {memo}
            </section>
          </div>
        </section>
        {panelOpen ? (
          <section className="pane side-panel" aria-labelledby="h-knowledge" id="knowledge-panel">
            <div className="pane-head">
              <h2 id="h-knowledge">関連ナレッジ</h2>
            </div>
            <div className="body">{knowledge}</div>
          </section>
        ) : null}
      </div>
    );
  } else if (mid) {
    body = (
      <div className="live-grid two">
        <section className="pane" aria-labelledby="h-transcript">
          <div className="pane-head">
            <h2 id="h-transcript">書き起こし</h2>
            <SegSearch id="seg-search" value={query} onChange={setQuery} />
          </div>
          {transcript}
        </section>
        <section className="pane">
          <div className="pane-head">
            <Tabs
              idPrefix="live"
              label="会議の補助"
              tabs={[
                { id: "questions", label: "アジェンダ" },
                { id: "memo", label: notes.length ? `メモ（${notes.length}）` : "メモ" },
                { id: "knowledge", label: knowledgeLabel },
              ]}
              active={["knowledge", "memo"].includes(tab) ? tab : "questions"}
              onChange={setTab}
            />
            {!["knowledge", "memo"].includes(tab) ? addButton : null}
          </div>
          <TabPanel idPrefix="live" id={["knowledge", "memo"].includes(tab) ? tab : "questions"} className="body">
            {tab === "knowledge" ? knowledge : tab === "memo" ? memo : questions}
          </TabPanel>
        </section>
      </div>
    );
  } else {
    body = (
      <section className="pane narrow">
        <div className="pane-head">
          <Tabs
            idPrefix="live"
            label="会議の表示"
            tabs={[
              { id: "transcript", label: "書き起こし" },
              { id: "questions", label: "アジェンダ" },
              { id: "memo", label: "メモ" },
              { id: "knowledge", label: newHits ? `資料（${newHits}）` : "資料" },
            ]}
            active={tab}
            onChange={setTab}
          />
        </div>
        <TabPanel idPrefix="live" id={tab} className="narrow-body">
          {tab === "transcript" ? (
            <>
              <div className="pane-tools">
                <SegSearch id="seg-search" value={query} onChange={setQuery} />
              </div>
              {transcript}
            </>
          ) : tab === "questions" ? (
            <div className="body">
              <div className="pane-tools">{addButton}</div>
              {questions}
            </div>
          ) : tab === "memo" ? (
            <div className="body">{memo}</div>
          ) : (
            <div className="body">{knowledge}</div>
          )}
        </TabPanel>
      </section>
    );
  }

  return (
    <div className="page live-page">
      <MeetingHead
        detail={detail}
        extra={
          <>
            <button className="btn ghost" type="button" onClick={() => setImporting(true)} title="ボイスレコーダーの音声や、文字起こし済みのテキストを足します">
              取り込む…
            </button>
            {wide ? (
              <button className="btn ghost" type="button" onClick={togglePanel} aria-expanded={panelOpen} aria-controls="knowledge-panel">
                {panelOpen ? "資料パネルを閉じる" : newHits ? `資料パネルを開く（新着 ${newHits}）` : "資料パネルを開く"}
              </button>
            ) : null}
          </>
        }
      />
      <Notices />
      <AssistBar detail={detail} />
      {body}
      {importing ? <ImportDialog detail={detail} onClose={() => setImporting(false)} /> : null}
    </div>
  );
}

const SCOPES: { id: ExportScope; label: string; note: string }[] = [
  { id: "summary", label: "要約のみ", note: "決定事項と次の作業を共有するとき" },
  { id: "qa", label: "要約＋アジェンダ", note: "議題ごとの結論・経過時間・メモも一緒に渡すとき" },
  { id: "full", label: "記録一式", note: "全文書き起こしと参照資料（場所を含む）まで残すとき" },
];

/** 出力範囲を選び、中身を確かめてから保存・コピーする。 */
function ExportDialog({ meetingId, onClose }: { meetingId: number; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [scope, setScope] = useState<ExportScope>("summary");
  const [preview, setPreview] = useState("");
  const [loading, setLoading] = useState(false);
  useEffect(() => {
    ref.current?.showModal();
  }, []);
  useEffect(() => {
    let alive = true;
    setLoading(true);
    request(`/api/meetings/${meetingId}/export?scope=${scope}`)
      .then((response) => response.text())
      .then((text) => alive && setPreview(text))
      .catch((err) => alive && setPreview(`プレビューを取得できません: ${errorMessage(err)}`))
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
  }, [meetingId, scope]);
  const contains = (word: string) => preview.includes(word);
  return (
    <dialog
      ref={ref}
      className="confirm wide"
      aria-labelledby="export-title"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <h2 className="dlg-title" id="export-title">
        議事録を書き出す
      </h2>
      <fieldset className="scope-pick">
        <legend>書き出す範囲</legend>
        {SCOPES.map((item) => (
          <label key={item.id} className="check">
            <input type="radio" name="scope" value={item.id} checked={scope === item.id} onChange={() => setScope(item.id)} />
            <span>
              <strong>{item.label}</strong> <span className="muted small">{item.note}</span>
            </span>
          </label>
        ))}
      </fieldset>
      <p className="muted small">
        含まれるもの: 要約
        {contains("## アジェンダ") ? "・アジェンダと結論" : ""}
        {contains("## 全文書き起こし") ? "・全文書き起こし" : ""}
        {contains("## 参照した資料") ? "・参照資料（保存場所を含む）" : ""} ／ {preview.length.toLocaleString()} 文字
      </p>
      <pre className="preview" aria-label="書き出す内容のプレビュー" aria-busy={loading}>
        {loading ? "読み込み中…" : preview}
      </pre>
      <div className="actions end">
        <button className="btn" type="button" onClick={onClose}>
          閉じる
        </button>
        <button className="btn" type="button" disabled={loading} onClick={() => void copyText(preview, "書き出す内容をコピーしました。").catch((err) => notify(err.message, "error"))}>
          コピー
        </button>
        <button
          className="btn primary"
          type="button"
          disabled={loading}
          onClick={() =>
            void exportMarkdown(meetingId, scope)
              .then(onClose)
              .catch((err) => notify(errorMessage(err), "error"))
          }
        >
          Markdown で保存
        </button>
      </div>
    </dialog>
  );
}

function SummaryPanel({ detail }: { detail: MeetingDetail }) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  const meeting = detail.meeting;
  if (editing) {
    return (
      <div>
        <label htmlFor="summary-editor" className="sr-only">
          要約（Markdown）
        </label>
        <textarea id="summary-editor" className="editor" value={text} onChange={(event) => setText(event.target.value)} autoFocus />
        <div className="actions">
          <button className="btn" type="button" onClick={() => setEditing(false)}>
            やめる
          </button>
          <button
            className="btn primary"
            type="button"
            onClick={() =>
              void guarded("summary", async () => {
                setDetail(await api<MeetingDetail>(`/api/meetings/${meeting.id}/summary`, { method: "PUT", body: { summary_md: text } }));
                setEditing(false);
                notify("要約を保存しました。", "info");
              })
            }
          >
            要約を保存
          </button>
        </div>
      </div>
    );
  }
  return (
    <div>
      {meeting.error ? (
        <div className="banner">
          自動要約に失敗しました（{meeting.error.replace(/[。.]\s*$/, "")}）。「要約を作り直す」でやり直せます。
        </div>
      ) : null}
      <div className="actions top">
        <button
          className="btn small"
          type="button"
          onClick={() => {
            setText(meeting.summary_md || "");
            setEditing(true);
          }}
        >
          要約を編集
        </button>
      </div>
      <div className="md" dangerouslySetInnerHTML={{ __html: markdown(meeting.summary_md || "要約はまだありません") }} />
    </div>
  );
}

function DoneView({ detail }: { detail: MeetingDetail }) {
  const [tab, setTab] = useState("summary");
  const [query, setQuery] = useState("");
  const [exporting, setExporting] = useState(false);
  const [playingId, setPlayingId] = useState<number | null>(null);
  const [flashId, setFlashId] = useState<number | null>(null);
  const [noAudio, setNoAudio] = useState(false);
  const [importing, setImporting] = useImportDialog(detail.meeting.id);
  const player = useRef<HTMLAudioElement>(null);
  const busyName = useBusy();
  const meeting = detail.meeting;
  const segments = detail.segments;

  const onSeek = useCallback((sec: number) => {
    const audio = player.current;
    if (!audio) return;
    audio.currentTime = sec;
    void audio.play().catch(() => {});
  }, []);
  const onEdit = useCallback((segment: Segment) => void editSegment(meetingStore.get().detail!, segment).catch((err) => notify(errorMessage(err), "error")), []);
  const onJump = useCallback(
    (item: Evidence) => {
      setTab("transcript");
      setQuery("");
      setFlashId(null);
      setTimeout(() => setFlashId(item.segment_id), 0);
      const audio = player.current;
      if (audio) audio.currentTime = item.start_sec;
    },
    [],
  );

  const followPlayer = () => {
    const audio = player.current;
    if (!audio || audio.paused) return;
    let current: number | null = null;
    for (const segment of segments) {
      if (segment.start_sec <= audio.currentTime + 0.05) current = segment.id;
      else break;
    }
    if (current !== playingId) setPlayingId(current);
  };

  const extra = (
    <>
      <button className="btn primary" type="button" onClick={() => setExporting(true)}>
        書き出す…
      </button>
      <button className="btn" type="button" onClick={() => void copyText(meeting.summary_md || "", "要約をコピーしました。").catch((err) => notify(err.message, "error"))}>
        要約をコピー
      </button>
      <button
        className="btn"
        type="button"
        disabled={detail.summarizing || !!busyName}
        onClick={() =>
          void guarded("resummarize", async () => {
            await api(`/api/meetings/${meeting.id}/resummarize`, { method: "POST" });
            patchDetail((current) => ({ ...current, summarizing: true }));
            notify("要約を作り直しています。", "info");
          })
        }
      >
        {detail.summarizing ? "要約を作成中…" : "要約を作り直す"}
      </button>
      <button className="btn" type="button" disabled={detail.summarizing} onClick={() => setImporting(true)} title="ボイスレコーダーの音声や、文字起こし済みのテキストを足して、要約を作り直します">
        取り込む…
      </button>
    </>
  );

  return (
    <div className="page done-page">
      <MeetingHead detail={detail} extra={extra} />
      <Notices />
      <div className="player-bar">
        <audio
          id="player"
          ref={player}
          controls
          preload="metadata"
          src={`/api/meetings/${meeting.id}/audio`}
          onError={() => setNoAudio(true)}
          onTimeUpdate={followPlayer}
          aria-label="会議の録音"
        />
        {noAudio ? <p className="muted small">録音はまだありません。</p> : null}
      </div>
      <Tabs
        idPrefix="done"
        label="議事録の表示"
        tabs={[
          { id: "summary", label: "要約・決定事項" },
          { id: "questions", label: "アジェンダ" },
          { id: "transcript", label: "全文書き起こし" },
          { id: "knowledge", label: "参照資料" },
        ]}
        active={tab}
        onChange={setTab}
      />
      <TabPanel idPrefix="done" id={tab} className="panel done-body">
        {tab === "summary" ? (
          <SummaryPanel detail={detail} />
        ) : tab === "questions" ? (
          <>
            <div className="actions top">
              <button className="btn small" type="button" onClick={() => void addQuestion(meeting.id).catch((err) => notify(errorMessage(err), "error"))}>
                ＋ 議題を追加
              </button>
            </div>
            <AgendaProgress detail={detail} now={meeting.duration_sec || 0} live={false} />
            <QuestionList meetingId={meeting.id} questions={detail.questions} activeId={null} onJump={onJump} now={meeting.duration_sec || 0} />
            <section className="memo-block" aria-labelledby="h-done-memo">
              <h2 id="h-done-memo">会議中のメモ（AI が発言から拾ったもの）</h2>
              <NotesPanel
                meetingId={meeting.id}
                notes={detail.notes || []}
                onJumpTime={(sec) => {
                  const segment = segments.find((item) => item.start_sec === sec);
                  if (segment) onJump({ segment_id: segment.id, start_sec: sec, quote: "" });
                }}
              />
            </section>
          </>
        ) : tab === "transcript" ? (
          <>
            <div className="pane-tools">
              <SegSearch id="seg-search" value={query} onChange={setQuery} />
            </div>
            <Transcript segments={segments} query={query} seekable playingId={playingId} flashId={flashId} onSeek={onSeek} onEdit={onEdit} follow={false} marked={markedIds(detail)} />
          </>
        ) : (
          <KnowledgePanel detail={detail} done />
        )}
      </TabPanel>
      {exporting ? <ExportDialog meetingId={meeting.id} onClose={() => setExporting(false)} /> : null}
      {importing ? <ImportDialog detail={detail} onClose={() => setImporting(false)} /> : null}
    </div>
  );
}
