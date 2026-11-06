import { useEffect, useRef, useState } from "react";
import { api, errorMessage, request } from "../api/client";
import type { Provider, Settings as SettingsData, Template, Transcriber, Upload } from "../api/types";
import { formatBytes } from "../api/client";
import { fmtLocal } from "../lib/format";
import { navigate } from "../lib/router";
import { keyReady, keySet, providerName } from "../state/actions";
import { guarded, useBusy } from "../state/busy";
import { confirmDialog, openDialog } from "../state/dialog";
import { clearSetupReturn, setupReturn } from "../state/draft";
import { clearNotice, notify } from "../state/notices";
import { Notices } from "../components/Notices";
import { captionAvailability, installLocalCaptions } from "../lib/captions";

type Form = {
  provider: Provider;
  transcriber: Transcriber;
  summary_prompt: string;
  knowledge_path: string;
  fess_enabled: boolean;
  fess_url: string;
};

const KEY_LINKS: Record<Provider, string> = {
  gemini: "https://aistudio.google.com/api-keys?hl=ja",
  openai: "https://platform.openai.com/api-keys",
  anthropic: "https://platform.claude.com/settings/keys",
};

const KEY_FIELDS: Record<Provider, { key: string; clear: string }> = {
  gemini: { key: "api_key", clear: "clear_api_key" },
  openai: { key: "openai_api_key", clear: "clear_openai_api_key" },
  anthropic: { key: "anthropic_api_key", clear: "clear_anthropic_api_key" },
};

function formOf(settings: SettingsData): Form {
  return {
    provider: settings.provider || "gemini",
    transcriber: settings.transcriber || "gemini",
    summary_prompt: settings.summary_prompt,
    knowledge_path: settings.knowledge_path,
    fess_enabled: settings.fess_enabled,
    fess_url: settings.fess_url,
  };
}

function modelLine(settings: SettingsData, form: Form): string {
  const check = settings.model_check || { running: false, error: null };
  const heard = settings.transcriber === "openai" ? settings.openai_transcribe_model : settings.model;
  const current =
    settings.provider === "anthropic"
      ? `文字起こし: ${heard || "未選択"} / 回答・要約: ${settings.anthropic_model || "未選択"}`
      : settings.provider === "openai"
        ? `文字起こし: ${settings.openai_transcribe_model || "未選択"} / 回答・要約: ${settings.openai_model || "未選択"}`
        : `使用モデル: ${settings.model || "未選択"}`;
  if (form.provider !== settings.provider || (form.provider === "anthropic" && form.transcriber !== settings.transcriber)) {
    return "保存すると、この提供元で使えるモデルを自動で選びます。";
  }
  if (check.running) return `使えるモデルを確認しています…（${current}）`;
  if (check.error) return `使えるモデルを自動選択できませんでした: ${check.error}（${current}）`;
  return keyReady(settings) ? `モデルは自動で選びます。${current}` : "キーを登録すると、そのキーで使えるモデルを自動で選びます。";
}

const FEE_NOTE =
  "このサービスの Free プランと、Gemini / OpenAI / Claude の API 料金・利用枠は別です。登録した自分のキーの契約によっては、提供元の料金が発生します。";

function KeyField({ provider, settings, onChanged }: { provider: Provider; settings: SettingsData; onChanged: (settings: SettingsData) => void }) {
  const [value, setValue] = useState("");
  const [show, setShow] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; message: string } | null>(null);
  const busyName = useBusy();
  const registered = keySet(settings, provider);
  const name = providerName(provider);
  const id = `key-${provider}`;
  const save = () =>
    guarded("key", async () => {
      if (!value.trim()) throw new Error("APIキーを入力してください");
      const body = { [KEY_FIELDS[provider].key]: value.trim() };
      onChanged(await api<SettingsData>("/api/settings", { method: "PUT", body }));
      setValue("");
      notify(`${name} の APIキーを登録しました。使えるモデルを確認しています。`, "info");
    });
  const check = () =>
    guarded("check", async () => {
      setResult({ ok: true, message: "確認しています。" });
      setResult(await api<{ ok: boolean; message: string }>("/api/settings/check-api-key", { method: "POST", body: { provider, api_key: value.trim() } }));
    });
  const clear = () =>
    guarded("clear", async () => {
      const ok = await confirmDialog(`登録した ${name} の APIキーを削除しますか？`, <p>削除すると、新しいキーを登録するまで {name} での文字起こしと要約ができません。</p>, "削除する", true);
      if (!ok) return;
      const body = { [KEY_FIELDS[provider].clear]: true };
      onChanged(await api<SettingsData>("/api/settings", { method: "PUT", body }));
      notify(`${name} の APIキーを削除しました。`, "info");
    });
  return (
    <div className="key-box">
      <div className={registered ? "note" : "banner"}>
        {registered ? `${name} の APIキーは登録済みです。値は再表示しません。差し替えるときだけ入力してください。` : `${name} の APIキーが未登録です。`}
      </div>
      <label htmlFor={id}>{name} の APIキー</label>
      <div className="mic-row">
        <input
          id={id}
          type={show ? "text" : "password"}
          autoComplete="off"
          spellCheck={false}
          value={value}
          placeholder={registered ? "変更するときだけ入力" : "APIキーを貼り付け"}
          onChange={(event) => setValue(event.target.value)}
          aria-describedby={`${id}-hint`}
        />
        <button className="btn" type="button" onClick={() => setShow(!show)} aria-pressed={show}>
          {show ? "隠す" : "表示"}
        </button>
      </div>
      <p className="hint" id={`${id}-hint`}>
        キーがない場合は
        <a className="text-link" href={KEY_LINKS[provider]} target="_blank" rel="noopener noreferrer">
          こちらで取得できます
        </a>
        。{FEE_NOTE}
      </p>
      <div className="actions">
        <button className="btn primary" type="button" disabled={!!busyName || !value.trim()} onClick={() => void save()}>
          {registered ? "キーを差し替える" : "キーを登録"}
        </button>
        <button className="btn" type="button" disabled={!!busyName} onClick={() => void check()}>
          キーが正しいかチェックする
        </button>
        {registered ? (
          <button className="btn" type="button" disabled={!!busyName} onClick={() => void clear()}>
            登録したキーを削除
          </button>
        ) : null}
      </div>
      {result ? <div className={result.ok ? "note" : "banner"}>{result.message}</div> : null}
    </div>
  );
}

function Uploads({ settings, onChanged }: { settings: SettingsData; onChanged: (settings: SettingsData) => void }) {
  const input = useRef<HTMLInputElement>(null);
  const busyName = useBusy();
  const refresh = async () => onChanged(await api<SettingsData>("/api/settings"));
  const upload = (files: FileList | null) =>
    guarded("upload", async () => {
      for (const file of Array.from(files || [])) {
        if (!/\.(md|txt|pdf|docx|xlsx)$/i.test(file.name)) throw new Error(`${file.name}: 取り込めるのは .md / .txt / .pdf / .docx / .xlsx です（.doc / .xls は docx / xlsx か PDF で保存し直してください）`);
        if (file.size > 8 * 1024 * 1024) throw new Error(`${file.name}: 8MB を超えるファイルは取り込めません`);
        await request("/api/knowledge/files", { method: "POST", raw: file, headers: { "X-File-Name": encodeURIComponent(file.name) } });
      }
      if (input.current) input.current.value = "";
      await refresh();
      notify("資料を追加しました。「索引を作り直す」で検索できるようになります。", "info");
    });
  const remove = (item: Upload) =>
    guarded("upload", async () => {
      const ok = await confirmDialog(`「${item.name}」を削除しますか？`, <p>索引からも外すには、削除のあと「索引を作り直す」を押してください。</p>, "削除する", true);
      if (!ok) return;
      await api(`/api/knowledge/files/${encodeURIComponent(item.name)}`, { method: "DELETE" });
      await refresh();
    });
  return (
    <div className="field">
      <label htmlFor="upload-input">資料をアップロード（.md / .txt / .pdf / .docx / .xlsx、8MB まで）</label>
      <input id="upload-input" ref={input} type="file" accept=".md,.txt,.pdf,.docx,.xlsx" multiple disabled={!!busyName} onChange={(event) => void upload(event.target.files)} />
      {settings.uploads?.length ? (
        <ul className="file-list">
          {settings.uploads.map((item) => (
            <li key={item.name}>
              <span className="file-name">{item.name}</span>
              <span className="muted small">
                {formatBytes(item.bytes)} ・ {fmtLocal(item.uploaded_at)}
              </span>
              <button className="btn small" type="button" onClick={() => void remove(item)} aria-label={`${item.name}を削除`}>
                削除
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="muted small">アップロードした資料はまだありません。</p>
      )}
    </div>
  );
}

function Templates() {
  const [templates, setTemplates] = useState<Template[]>([]);
  const load = () => api<{ templates: Template[] }>("/api/templates").then((data) => setTemplates(data.templates));
  useEffect(() => {
    void load().catch(() => {});
  }, []);
  const edit = (tpl: Template) =>
    guarded("tpl", async () => {
      const values = await openDialog({
        title: "テンプレートを編集",
        fields: [
          { name: "name", label: "テンプレート名", value: tpl.name },
          { name: "questions", label: "議題（1行に1つ）", value: tpl.questions.join("\n"), type: "textarea", rows: 8 },
        ],
        confirm: "保存",
      });
      if (!values) return;
      const questions = values.questions.split("\n").map((line) => line.trim()).filter(Boolean);
      await api(`/api/templates/${tpl.id}`, { method: "PUT", body: { name: values.name, questions } });
      await load();
      notify("テンプレートを保存しました。", "info");
    });
  const remove = (tpl: Template) =>
    guarded("tpl", async () => {
      const ok = await confirmDialog(`テンプレート「${tpl.name}」を削除しますか？`, <p className="muted">作成済みの会議には影響しません。</p>, "削除する", true);
      if (!ok) return;
      await api(`/api/templates/${tpl.id}`, { method: "DELETE" });
      await load();
    });
  return (
    <section className="panel settings-section" aria-labelledby="sec-templates">
      <h2 id="sec-templates">テンプレート</h2>
      <p className="muted small">新しい会議の画面で「テンプレートとして保存」すると、ここに並びます。</p>
      <div className="list" id="template-list">
        {templates.length ? (
          templates.map((tpl) => (
            <div className="row" key={tpl.id}>
              <div className="row-text">
                <strong>{tpl.name}</strong>
                <span className="muted">{tpl.questions.join(" / ")}</span>
              </div>
              <button className="btn small" type="button" onClick={() => void edit(tpl)} aria-label={`テンプレート「${tpl.name}」を編集`}>
                編集
              </button>
              <button className="btn small" type="button" onClick={() => void remove(tpl)} aria-label={`テンプレート「${tpl.name}」を削除`}>
                削除
              </button>
            </div>
          ))
        ) : (
          <div className="empty">テンプレートはまだありません。</div>
        )}
      </div>
    </section>
  );
}

const AVAILABILITY: Record<string, string> = {
  available: "この端末で使えます。",
  downloadable: "日本語の言語パックを入れると使えます（Chrome がダウンロードします）。",
  downloading: "日本語の言語パックをダウンロードしています。",
  unavailable: "この端末・Chrome では、日本語の認識が使えません。",
  unsupported: "このブラウザは音声認識に対応していません（デスクトップの Chrome をお使いください）。",
};

/** リアルタイム文字起こし（Chrome の速報字幕, #104）。選んだら、すぐ保存する。 */
function RealtimeSection({ settings, onChanged }: { settings: SettingsData; onChanged: (settings: SettingsData) => void }) {
  const [status, setStatus] = useState("");
  const [busy, setBusy] = useState(false);
  // 押した時点で選んだ状態にする（保存の応答を待たない）。
  const [pending, setPending] = useState<"off" | "local" | "cloud" | null>(null);
  const mode = pending ?? (settings.realtime_captions || "off");
  const check = async (target: "local" | "cloud") => {
    setStatus("確認しています…");
    const result = await captionAvailability(target);
    setStatus(AVAILABILITY[result] || result);
    return result;
  };
  const choose = async (next: "off" | "local" | "cloud") => {
    setBusy(true);
    setPending(next);
    try {
      onChanged(await api<SettingsData>("/api/settings", { method: "PUT", body: { realtime_captions: next } }));
      if (next !== "off") await check(next);
      else setStatus("");
    } catch (err) {
      notify(errorMessage(err), "error");
    } finally {
      setPending(null);
      setBusy(false);
    }
  };
  const install = async () => {
    setBusy(true);
    setStatus("日本語の言語パックを入れています…");
    const ok = await installLocalCaptions();
    setBusy(false);
    if (ok) await check("local");
    else setStatus("言語パックを入れられませんでした。Chrome を最新にして、もう一度お試しください。");
  };
  return (
    <section className="panel settings-section" aria-labelledby="sec-realtime">
      <h2 id="sec-realtime">リアルタイム文字起こし（速報字幕）</h2>
      <p className="muted small">
        APIキーで文字起こしする会議では、録音中に Chrome の速報を出します。API の文字起こしが届いた部分の速報は置き換わり、速報自体は残りません。Chrome の音声認識だけを選んだ会議では、この認識を書き起こしとして保存します。
      </p>
      {!settings.realtime_allowed ? (
        <div className="banner">いまのプランでは、リアルタイム文字起こしは使えません。</div>
      ) : (
        <>
          <fieldset className="field provider-pick">
            <legend>速報字幕</legend>
            <label className="check">
              <input type="radio" name="realtime" checked={mode === "off"} disabled={busy} onChange={() => void choose("off")} />
              使わない
            </label>
            <label className="check">
              <input type="radio" name="realtime" checked={mode === "local"} disabled={busy} onChange={() => void choose("local")} />
              使う（端末内で認識。音声は端末の外に出ません）
            </label>
            {settings.cloud_speech_allowed || mode === "cloud" ? (
              <label className="check">
                <input type="radio" name="realtime" checked={mode === "cloud"} disabled={busy || !settings.cloud_speech_allowed} onChange={() => void choose("cloud")} />
                使う（Google のサーバで認識。音声が Google に送られます。運用者が許可した場合だけ）
              </label>
            ) : null}
          </fieldset>
          <div className="actions">
            {mode !== "off" ? (
              <button className="btn small" type="button" disabled={busy} onClick={() => void check(mode)}>
                この端末で使えるか確認
              </button>
            ) : null}
            {mode === "local" && /言語パックを入れると/.test(status) ? (
              <button className="btn small primary" type="button" disabled={busy} onClick={() => void install()}>
                日本語の言語パックを入れる
              </button>
            ) : null}
          </div>
          {status ? <p className="note small" aria-live="polite">{status}</p> : null}
        </>
      )}
    </section>
  );
}

export function Settings() {
  const [settings, setSettings] = useState<SettingsData | null>(null);
  const [form, setForm] = useState<Form | null>(null);
  const [fessToken, setFessToken] = useState("");
  const [fessResult, setFessResult] = useState<{ ok: boolean; message: string } | null>(null);
  const busyName = useBusy();
  const returnTo = setupReturn();

  const adopt = (next: SettingsData, resetForm = false) => {
    setSettings(next);
    if (resetForm) setForm(formOf(next));
  };

  useEffect(() => {
    api<SettingsData>("/api/settings")
      .then((next) => adopt(next, true))
      .catch((err) => notify(errorMessage(err), "error"));
  }, []);

  // キーを登録したら「先にキーを登録してください」の案内は役目を終える。
  useEffect(() => {
    if (settings && keyReady(settings)) clearNotice("setup");
  }, [settings]);

  // モデルの確認中は、結果が出るまで表示を更新する。
  useEffect(() => {
    if (!settings?.model_check?.running) return;
    const timer = setInterval(() => {
      api<SettingsData>("/api/settings")
        .then((next) => setSettings(next))
        .catch(() => {});
    }, 2000);
    return () => clearInterval(timer);
  }, [settings?.model_check?.running]);

  if (!settings || !form) {
    return (
      <div className="page">
        <Notices />
        <p className="muted">読み込み中</p>
      </div>
    );
  }

  const saved = formOf(settings);
  const dirty = JSON.stringify(saved) !== JSON.stringify(form) || !!fessToken;
  const update = (patch: Partial<Form>) => setForm({ ...form, ...patch });
  const index = settings.index || { docs: 0, chunks: 0, errors: 0 };

  const save = () =>
    guarded("save", async () => {
      const pathChanged = saved.knowledge_path !== form.knowledge_path.trim();
      const body: Record<string, unknown> = { ...form };
      if (fessToken) body.fess_token = fessToken;
      const next = await api<SettingsData>("/api/settings", { method: "PUT", body });
      adopt(next, true);
      setFessToken("");
      notify(pathChanged && form.knowledge_path.trim() ? "保存しました。ナレッジフォルダを変えたので「索引を作り直す」を押してください。" : "保存しました", pathChanged ? "warn" : "info");
    });

  const reindex = () =>
    guarded("reindex", async () => {
      notify("索引を作っています。ファイルが多いと時間がかかります。", "info", "reindex");
      const result = await api<{ docs: number; chunks: number; errors: number }>("/api/knowledge/reindex", { method: "POST" });
      adopt(await api<SettingsData>("/api/settings"));
      notify(`索引しました。ファイル ${result.docs}、断片 ${result.chunks}、失敗 ${result.errors}`, result.errors ? "warn" : "info", "reindex");
    });

  const checkFess = () =>
    guarded("fess", async () => {
      setFessResult({ ok: true, message: "確認しています。" });
      setFessResult(await api<{ ok: boolean; message: string }>("/api/settings/check-fess", { method: "POST", body: { fess_url: form.fess_url, fess_token: fessToken } }));
    });

  const clearFess = () =>
    guarded("fess", async () => {
      const ok = await confirmDialog("FESS のアクセストークンを削除しますか？", null, "削除する", true);
      if (!ok) return;
      adopt(await api<SettingsData>("/api/settings", { method: "PUT", body: { clear_fess_token: true } }));
      notify("トークンを削除しました。", "info");
    });

  return (
    <div className="page settings-page">
      <header className="page-head">
        <div>
          <h1>設定</h1>
          <p className="muted">この設定はあなたのアカウントだけに使われます。APIキーの値は画面に戻しません。</p>
        </div>
      </header>
      <Notices />
      {returnTo && keyReady(settings) ? (
        <div className="note return-box">
          <span>APIキーを登録しました。準備していた会議に戻って開始できます。</span>
          <button
            className="btn primary"
            type="button"
            onClick={() => {
              clearSetupReturn();
              clearNotice("setup");
              navigate(returnTo);
            }}
          >
            会議の準備に戻る
          </button>
        </div>
      ) : null}

      <section className="panel settings-section" aria-labelledby="sec-ai">
        <h2 id="sec-ai">AI接続</h2>
        <fieldset className="field provider-pick">
          <legend>回答候補・要約に使う AI</legend>
          {(["gemini", "openai", "anthropic"] as Provider[]).map((provider) => (
            <label key={provider} className="check">
              <input type="radio" name="provider" value={provider} checked={form.provider === provider} onChange={() => update({ provider })} />
              {providerName(provider)}
              {provider === settings.provider ? <span className="muted small">（使用中）</span> : null}
            </label>
          ))}
          {form.provider !== settings.provider ? <p className="hint">「保存」を押すと切り替わります。</p> : null}
          <p className="hint">Gemini と OpenAI は文字起こしもします。Cursor は外部のアプリから呼べる文章生成の API を公開していないため、選べません。</p>
        </fieldset>
        <KeyField key={form.provider} provider={form.provider} settings={settings} onChanged={(next) => adopt(next)} />
        {form.provider === "anthropic" ? (
          <>
            <fieldset className="field provider-pick">
              <legend>文字起こしに使う AI</legend>
              <p className="hint">Claude は音声を受け付けないため、録音の文字起こしは Gemini か OpenAI で行い、その文を Claude に渡します。</p>
              {(["gemini", "openai"] as Transcriber[]).map((transcriber) => (
                <label key={transcriber} className="check">
                  <input type="radio" name="transcriber" value={transcriber} checked={form.transcriber === transcriber} onChange={() => update({ transcriber })} />
                  {providerName(transcriber)}
                  {settings.provider === "anthropic" && transcriber === settings.transcriber ? <span className="muted small">（使用中）</span> : null}
                </label>
              ))}
              {form.transcriber !== settings.transcriber ? <p className="hint">「保存」を押すと切り替わります。</p> : null}
            </fieldset>
            <KeyField key={`ears-${form.transcriber}`} provider={form.transcriber} settings={settings} onChanged={(next) => adopt(next)} />
          </>
        ) : null}
        <p className="muted small" aria-live="polite">
          {modelLine(settings, form)}
        </p>
      </section>

      <RealtimeSection settings={settings} onChanged={(next) => adopt(next)} />

      <section className="panel settings-section" aria-labelledby="sec-minutes">
        <h2 id="sec-minutes">議事録の作り方</h2>
        <div className="field">
          <label htmlFor="summary-prompt">要約プロンプト</label>
          <textarea id="summary-prompt" rows={8} value={form.summary_prompt} onChange={(event) => update({ summary_prompt: event.target.value })} />
          <div className="actions">
            <button className="btn small" type="button" onClick={() => update({ summary_prompt: settings.default_summary_prompt })}>
              プロンプトを初期化
            </button>
          </div>
        </div>
      </section>

      <section className="panel settings-section" aria-labelledby="sec-local">
        <h2 id="sec-local">ローカル資料</h2>
        <p className="muted small">会議中に関連する資料を探すために使います。資料の本文は AI に送りません（送るのは短い検索語だけです）。</p>
        <Uploads settings={settings} onChanged={(next) => adopt(next)} />
        {settings.knowledge_root ? (
          <div className="field">
            <label htmlFor="knowledge-path">サーバ上の資料フォルダ</label>
            <input id="knowledge-path" type="text" value={form.knowledge_path} placeholder={settings.knowledge_root} onChange={(event) => update({ knowledge_path: event.target.value })} aria-describedby="knowledge-path-hint" />
            <p className="hint" id="knowledge-path-hint">
              指定できるのは {settings.knowledge_root} の配下だけです。変えたら保存してから索引を作り直してください。
            </p>
          </div>
        ) : null}
        <div className="index-box">
          <p className="muted">
            索引 {index.docs} ファイル / {index.chunks} 断片 / 失敗 {index.errors}
          </p>
          <button className="btn" type="button" disabled={!!busyName} onClick={() => void reindex()}>
            索引を作り直す
          </button>
        </div>
        <p className="hint">保存済みの資料（アップロードとフォルダ）から索引を作ります。未保存の変更は先に保存してください。</p>
        {settings.index_errors?.length ? (
          <details className="small">
            <summary>索引できなかったファイル（{index.errors} 件）</summary>
            <ul>
              {settings.index_errors.map((item) => (
                <li key={item.path}>
                  <code>{item.path}</code> — {item.error}
                </li>
              ))}
            </ul>
          </details>
        ) : null}
      </section>

      <section className="panel settings-section" aria-labelledby="sec-fess">
        <h2 id="sec-fess">外部検索（FESS）</h2>
        <label className="check">
          <input type="checkbox" checked={form.fess_enabled} onChange={(event) => update({ fess_enabled: event.target.checked })} aria-controls="fess-detail" aria-expanded={form.fess_enabled} />
          FESS も検索する
        </label>
        {form.fess_enabled ? (
          <div id="fess-detail">
            <div className="field">
              <label htmlFor="fess-url">FESS の URL</label>
              <input id="fess-url" type="text" value={form.fess_url} placeholder="http://127.0.0.1:8080" onChange={(event) => update({ fess_url: event.target.value })} />
            </div>
            <div className="field">
              <label htmlFor="fess-token">FESS アクセストークン{settings.fess_token_set ? "（設定済み）" : ""}</label>
              <input id="fess-token" type="password" autoComplete="off" value={fessToken} placeholder="空欄なら変更しない" onChange={(event) => setFessToken(event.target.value)} />
            </div>
            <div className="actions">
              <button className="btn" type="button" disabled={!!busyName} onClick={() => void checkFess()}>
                FESS に接続できるか確かめる
              </button>
              {settings.fess_token_set ? (
                <button className="btn" type="button" disabled={!!busyName} onClick={() => void clearFess()}>
                  トークンを削除
                </button>
              ) : null}
            </div>
            {fessResult ? <div className={fessResult.ok ? "note" : "banner"}>{fessResult.message}</div> : null}
          </div>
        ) : (
          <p className="muted small">使わない場合はこのままで構いません。</p>
        )}
      </section>

      <div className={`save-bar ${dirty ? "dirty" : ""}`} role="region" aria-label="設定の保存">
        <span aria-live="polite">{dirty ? "未保存の変更があります" : "変更はすべて保存済みです"}</span>
        <div className="actions">
          <button className="btn" type="button" disabled={!dirty} onClick={() => { setForm(saved); setFessToken(""); }}>
            変更を取り消す
          </button>
          <button className="btn primary" type="button" disabled={!dirty || !!busyName} onClick={() => void save()}>
            保存
          </button>
        </div>
      </div>

      <Templates />
    </div>
  );
}
