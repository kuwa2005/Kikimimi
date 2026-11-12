import { useEffect, useRef, useState } from "react";
import { api, errorMessage } from "../api/client";
import type { Folder, MeetingDetail, Settings, Template } from "../api/types";
import { pref, PREF_MIC, PREF_TAB, sessionSet } from "../lib/prefs";
import { useStore } from "../lib/store";
import { devices, ensureAudio, listDevices, recorderState, secureContextProblem, setCaptureOptions, stopStream } from "../recorder/recorder";
import { beginMeeting, beginMeetingPaused, keyReady, saveMeeting } from "../state/actions";
import { guarded, useBusy } from "../state/busy";
import { openDialog } from "../state/dialog";
import { emptyForm, formKey, loadForm, saveForm, type MeetingForm as Form } from "../state/draft";
import { setDetail } from "../state/meeting";
import { notify } from "../state/notices";
import { Notices } from "../components/Notices";
import { Meter } from "../components/Meter";
import { EnhanceToggle } from "../components/Enhance";

function fromDetail(detail: MeetingDetail): Form {
  const questions = detail.questions.map((q) => q.body);
  return {
    title: detail.meeting.title,
    participants: detail.meeting.participants || "",
    templateId: "",
    questions: questions.length ? questions : [""],
    minutes: detail.questions.length ? detail.questions.map((q) => (q.minutes ? String(q.minutes) : "")) : [""],
    folderId: detail.meeting.folder_id != null ? String(detail.meeting.folder_id) : "",
    speech: detail.meeting.speech || "api",
    transcriptOnly: !!detail.meeting.transcript_only,
  };
}

/** 新しい会議と下書きの画面。入力はタブの間保持し、設定への往復やテンプレート保存で消さない。 */
export function MeetingForm({ detail }: { detail: MeetingDetail | null }) {
  const meetingId = detail ? detail.meeting.id : null;
  const key = formKey(meetingId);
  const [form, setForm] = useState<Form>(() => loadForm(key) || (detail ? fromDetail(detail) : emptyForm()));
  const [templates, setTemplates] = useState<Template[]>([]);
  const [folders, setFolders] = useState<Folder[]>([]);
  const [mic, setMic] = useState(() => pref(PREF_MIC));
  const [includeTab, setIncludeTab] = useState(() => pref(PREF_TAB) === "1");
  const deviceList = useStore(devices);
  const rec = useStore(recorderState);
  const busyName = useBusy();
  const testing = rec.streamOpen && !rec.capturing;
  const problem = secureContextProblem();
  const speechTouched = useRef(false);

  useEffect(() => saveForm(key, form), [key, form]);
  useEffect(() => {
    let alive = true;
    api<Settings>("/api/settings")
      .then((settings) => {
        if (!alive || keyReady(settings) || speechTouched.current) return;
        setForm((current) => (current.speech === "api" ? { ...current, speech: "chrome" } : current));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [key]);

  useEffect(() => {
    api<{ templates: Template[] }>("/api/templates")
      .then((data) => setTemplates(data.templates))
      .catch(() => {});
    api<{ folders: Folder[] }>("/api/folders")
      .then((data) => setFolders(data.folders))
      .catch(() => {});
    void listDevices();
    return () => {
      // テスト中のマイクは、画面を離れたら離す。録音中はそのまま。
      if (!recorderState.get().capturing && recorderState.get().streamOpen) stopStream();
    };
  }, []);

  const update = (patch: Partial<Form>) => setForm((current) => ({ ...current, ...patch }));
  const setQuestion = (index: number, value: string) =>
    setForm((current) => ({ ...current, questions: current.questions.map((q, i) => (i === index ? value : q)) }));

  const pickTemplate = (id: string) => {
    const picked = templates.find((tpl) => String(tpl.id) === id);
    update({ templateId: id, ...(picked ? { questions: picked.questions.slice(), minutes: picked.questions.map(() => "") } : {}) });
  };

  const saveTemplate = async () => {
    const questions = form.questions.map((q) => q.trim()).filter(Boolean);
    if (!questions.length) throw new Error("テンプレートにする議題を1件以上入れてください");
    const values = await openDialog({
      title: "テンプレートとして保存",
      body: <p className="muted small">{questions.join(" / ")}</p>,
      fields: [{ name: "name", label: "テンプレート名" }],
      confirm: "保存",
    });
    if (!values || !values.name.trim()) return;
    const created = await api<Template>("/api/templates", { method: "POST", body: { name: values.name, questions } });
    setTemplates((list) => [created, ...list]);
    // 入力中の会議の準備はそのまま残す。
    notify(`テンプレート「${values.name.trim()}」を保存しました。`, "info");
  };

  const toggleTest = async () => {
    if (rec.capturing) throw new Error("録音中はテストできません");
    if (testing) {
      stopStream();
      return;
    }
    setCaptureOptions(mic, false);
    await ensureAudio();
    await listDevices();
  };

  const onMicChange = (value: string) => {
    setMic(value);
    setCaptureOptions(value, includeTab);
    if (testing) {
      stopStream();
      setCaptureOptions(value, false);
      void ensureAudio().catch((err) => notify(errorMessage(err), "error"));
    }
  };

  const begin = () =>
    guarded("begin", async () => {
      setCaptureOptions(mic, includeTab);
      await beginMeeting(form, meetingId);
    });

  // PC で録音せず、ボイスレコーダーの音声や書き起こしだけで会議を作る（#96）。
  const fromFile = () =>
    guarded("import", async () => {
      const saved = await saveMeeting(form, meetingId);
      const id = saved.meeting.id;
      await api(`/api/meetings/${id}/start`, { method: "POST" });
      await api(`/api/meetings/${id}/pause`, { method: "POST" });
      sessionSet("kikimimi.openImport", id);
      location.hash = `#/m/${id}`;
    });

  const beginPaused = () => guarded("begin-paused", () => beginMeetingPaused(form, meetingId));

  const saveDraft = () =>
    guarded("save", async () => {
      const saved = await saveMeeting(form, meetingId);
      if (meetingId == null) {
        location.hash = `#/m/${saved.meeting.id}`;
      } else {
        setDetail(saved);
        notify("下書きを保存しました。", "info");
      }
    });

  const inputs = deviceList.filter((device) => device.deviceId && device.deviceId !== "default");
  const prefix = meetingId == null ? "new" : `draft-${meetingId}`;
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>{meetingId == null ? "新しい会議" : form.title || "下書き"}</h1>
          <p className="muted">{meetingId == null ? "アジェンダ（議題）を先に置いてから、会議を始めます。" : "下書きです。開始すると会議が始まります。"}</p>
        </div>
      </header>
      <Notices />
      <form
        className="panel form"
        onSubmit={(event) => {
          event.preventDefault();
          void begin();
        }}
      >
        <div className="field">
          <label htmlFor={`${prefix}-title`}>会議タイトル</label>
          <input id={`${prefix}-title`} type="text" value={form.title} placeholder={meetingId == null ? "未入力なら現在日時" : ""} onChange={(event) => update({ title: event.target.value })} />
        </div>
        <div className="field">
          <label htmlFor={`${prefix}-participants`}>参加者</label>
          <input id={`${prefix}-participants`} type="text" value={form.participants} placeholder="例: 田中、佐藤、鈴木" onChange={(event) => update({ participants: event.target.value })} />
        </div>
        <div className="field">
          <label htmlFor={`${prefix}-folder`}>フォルダ</label>
          <select
            id={`${prefix}-folder`}
            value={form.folderId || ""}
            onChange={(event) => {
              const value = event.target.value;
              update({ folderId: value });
              // 下書きは、その場でフォルダを移す。
              if (meetingId != null) {
                void api(`/api/meetings/${meetingId}/folder`, { method: "POST", body: { folder_id: value ? Number(value) : null } }).catch((err) =>
                  notify(errorMessage(err), "error"),
                );
              }
            }}
          >
            <option value="">フォルダなし</option>
            {folders.map((folder) => (
              <option key={folder.id} value={String(folder.id)}>
                {folder.name}
              </option>
            ))}
          </select>
          {!folders.length ? <p className="hint">フォルダはホームの「＋ フォルダ」で作れます。</p> : null}
        </div>
        <div className="field">
          <label htmlFor={`${prefix}-template`}>テンプレート</label>
          <select id={`${prefix}-template`} value={form.templateId} onChange={(event) => pickTemplate(event.target.value)} aria-describedby={`${prefix}-template-hint`}>
            <option value="">テンプレートを使わない</option>
            {templates.map((tpl) => (
              <option key={tpl.id} value={String(tpl.id)}>
                {tpl.name}
              </option>
            ))}
          </select>
          <p className="hint" id={`${prefix}-template-hint`}>
            選ぶと、下のアジェンダがテンプレートの内容に置き換わります。
          </p>
        </div>
        <fieldset className="field">
          <legend>アジェンダ（議題）</legend>
          <p className="hint">
            この会議で話し合うこと・相手から聞き出したいことを、進める順に書きます。目安時間（分）を入れると、会議中に議題ごとの経過と残りを表示します（任意）。
            会議中は AI が発言から「いまの議題」と回答・結論の候補を作り、根拠の発言に印を付けます。「完了して次へ」で次の議題に進めます。
            空でも始められます（話の流れから AI が確かめるべきことを足します）。件数に上限はありません。
          </p>
          {form.questions.map((question, index) => (
            <div className="q-row agenda-row" key={index}>
              <label htmlFor={`${prefix}-q-${index}`} className="q-label">
                議題{index + 1}
              </label>
              <input id={`${prefix}-q-${index}`} type="text" value={question} placeholder="話し合うこと・確かめたいこと" onChange={(event) => setQuestion(index, event.target.value)} />
              <label htmlFor={`${prefix}-m-${index}`} className="sr-only">
                議題{index + 1}の目安時間（分）
              </label>
              <input
                id={`${prefix}-m-${index}`}
                className="minutes-input"
                type="number"
                min={0}
                inputMode="numeric"
                placeholder="分"
                title="目安時間（分）"
                value={form.minutes?.[index] || ""}
                onChange={(event) =>
                  setForm((current) => {
                    const minutes = current.questions.map((_, i) => current.minutes?.[i] || "");
                    minutes[index] = event.target.value;
                    return { ...current, minutes };
                  })
                }
              />
              <button
                type="button"
                className="btn"
                aria-label={`議題${index + 1}を削除`}
                onClick={() =>
                  setForm((current) => {
                    const minutes = current.questions.map((_, i) => current.minutes?.[i] || "");
                    if (current.questions.length <= 1) return { ...current, questions: [""], minutes: [""] };
                    return { ...current, questions: current.questions.filter((_, i) => i !== index), minutes: minutes.filter((_, i) => i !== index) };
                  })
                }
              >
                削除
              </button>
            </div>
          ))}
          <button type="button" className="btn" onClick={() => update({ questions: [...form.questions, ""], minutes: [...form.questions.map((_, i) => form.minutes?.[i] || ""), ""] })}>
            議題を追加
          </button>
        </fieldset>
        <fieldset className="field">
          <legend>文字起こし</legend>
          <label className="check">
            <input
              type="radio"
              name={`${prefix}-speech`}
              checked={form.speech === "api"}
              onChange={() => {
                speechTouched.current = true;
                update({ speech: "api" });
              }}
            />
            APIキーで文字起こしする
          </label>
          <label className="check">
            <input
              type="radio"
              name={`${prefix}-speech`}
              checked={form.speech === "chrome"}
              onChange={() => {
                speechTouched.current = true;
                update({ speech: "chrome", transcriptOnly: false });
              }}
            />
            Chrome の音声認識（APIキー不要）
          </label>
          <label className="check">
            <input
              type="radio"
              name={`${prefix}-speech`}
              checked={form.speech === "off"}
              onChange={() => {
                speechTouched.current = true;
                update({ speech: "off", transcriptOnly: false });
              }}
            />
            録音だけ（文字起こしは終了時に選べます）
          </label>
          {form.speech === "api" ? (
            <label className="check">
              <input type="checkbox" checked={form.transcriptOnly} onChange={(event) => update({ transcriptOnly: event.target.checked })} />
              文字起こしだけにする（回答やナレッジは付けない）
            </label>
          ) : null}
          <p className="hint">
            {form.speech === "chrome"
              ? "Chrome の音声認識で書き起こします。APIキーは不要です。端末内の言語パックがあるときは端末内で、無いときは Chrome 標準の音声認識を使います。回答・要約用のキー（Claude など）を登録していれば、書き起こしの文字からアジェンダの回答も更新します（音声は送りません）。"
              : form.speech === "off"
                ? "録音だけ残します。終了時に API で文字起こしするか、そのまま議事録にするかを選べます。"
                : "Chrome の速報を出しながら、API で精度の高い文字起こしをします。"}
          </p>
        </fieldset>
        <fieldset className="field">
          <legend>マイク</legend>
          {problem ? <div className="banner error">{problem}</div> : null}
          <div className="mic-row">
            <label htmlFor={`${prefix}-mic`} className="sr-only">
              使うマイク
            </label>
            <select id={`${prefix}-mic`} value={mic} onChange={(event) => onMicChange(event.target.value)}>
              <option value="">既定のマイク</option>
              {inputs.map((device, index) => (
                <option key={device.deviceId} value={device.deviceId}>
                  {device.label || `マイク ${index + 1}`}
                </option>
              ))}
            </select>
            <button type="button" className="btn" onClick={() => void toggleTest().catch((err) => notify(errorMessage(err), "error"))}>
              {testing ? "テストをやめる" : "マイクをテスト"}
            </button>
            <Meter active={testing} label="マイクのテストの音量" />
            <EnhanceToggle />
          </div>
          <p className="hint">名前が出ないときは「マイクをテスト」を押して使用を許可してください。</p>
          <label className="check">
            <input type="checkbox" checked={includeTab} onChange={(event) => setIncludeTab(event.target.checked)} />
            オンライン会議の相手の声も録る（会議を開いているタブの音声を共有します）
          </label>
        </fieldset>
        <div className="actions">
          <button className="btn" type="button" onClick={() => void saveTemplate().catch((err) => notify(errorMessage(err), "error"))}>
            テンプレートとして保存
          </button>
          <button className="btn" type="button" disabled={!!busyName} onClick={() => void saveDraft()}>
            下書きを保存
          </button>
          <button className="btn" type="button" disabled={!!busyName} onClick={() => void fromFile()} title="ボイスレコーダーの音声や、文字起こし済みのテキストから議事録を作ります">
            録音せずにファイルから作る
          </button>
          <button className="btn primary" type="button" disabled={!!busyName} onClick={() => void beginPaused()} title="会議を始めますが、録音はしません。あとで「再開」か「取り込む…」で録音を足せます">
            {busyName === "begin-paused" ? "開始しています…" : "会議開始（録音一時停止）"}
          </button>
          <button className="btn danger" type="submit" disabled={!!busyName}>
            {busyName === "begin" ? "開始しています…" : "会議開始（録音開始）"}
          </button>
        </div>
      </form>
    </div>
  );
}
