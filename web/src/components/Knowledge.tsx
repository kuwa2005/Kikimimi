import { memo, useState } from "react";
import { api, errorMessage, formatBytes, request } from "../api/client";
import type { Hit, MeetingDetail, MeetingDoc } from "../api/types";
import { isUrl, shortLocator } from "../lib/format";
import { copyText } from "../state/actions";
import { patchDetail, refreshDetail } from "../state/meeting";
import { notify } from "../state/notices";

// 資料カードは「タイトル → 要点 → 短い出典」の順。長い場所は短くして、必要なら全文を出す。
const HitCard = memo(function HitCard({ meetingId, hit }: { meetingId: number; hit: Hit }) {
  const [more, setMore] = useState(false);
  const [where, setWhere] = useState(false);
  const url = isUrl(hit.locator);
  const long = hit.snippet.length > 120;
  const togglePin = async () => {
    try {
      const result = await api<{ hits: Hit[] }>(`/api/meetings/${meetingId}/hits/${hit.id}/pin`, { method: "POST", body: { pinned: !hit.pinned } });
      patchDetail((detail) => ({ ...detail, hits: result.hits }));
    } catch (err) {
      notify(errorMessage(err), "error");
    }
  };
  return (
    <article className={`hit ${hit.pinned ? "pinned" : ""}`}>
      <h3 className="hit-title">
        {hit.pinned ? (
          <span className="badge pin">
            <span aria-hidden="true">📌</span> ピン留め済み
          </span>
        ) : null}
        {url ? (
          <a className="text-link" href={hit.locator} target="_blank" rel="noopener noreferrer">
            {hit.title}
          </a>
        ) : (
          <span>{hit.title}</span>
        )}
      </h3>
      <p className={`hit-snippet ${more || !long ? "" : "clamp"}`}>{hit.snippet}</p>
      {long ? (
        <button type="button" className="link-btn small" onClick={() => setMore(!more)} aria-expanded={more}>
          {more ? "たたむ" : "続きを読む"}
        </button>
      ) : null}
      <p className="hit-source muted small">
        <span>{hit.source === "fess" ? "FESS" : hit.source === "customer" ? "顧客資料" : "ローカル"}</span> ・ <span title={hit.locator}>{shortLocator(hit.locator)}</span> ・ 検索語「{hit.query}」{" "}
        <button type="button" className="link-btn" onClick={() => setWhere(!where)} aria-expanded={where}>
          {where ? "場所を隠す" : "場所を表示"}
        </button>
      </p>
      {where ? <p className="hit-path small">{hit.locator}</p> : null}
      <div className="mini-actions">
        <button className="btn small" type="button" onClick={() => void togglePin()} aria-pressed={hit.pinned} aria-label={`${hit.title}を${hit.pinned ? "ピン留めから外す" : "ピン留めする"}`}>
          {hit.pinned ? "ピンを外す" : "ピン留め"}
        </button>
        {!url ? (
          <button className="btn small" type="button" onClick={() => void copyText(hit.locator, "パスをコピーしました。").catch((err) => notify(err.message, "error"))}>
            パスをコピー
          </button>
        ) : null}
      </div>
    </article>
  );
});

const DOC_ACCEPT = ".pdf,.docx,.xlsx,.txt,.md,.doc,.xls";

/** 顧客から受け取った資料（#97）。会議中の資料検索と、要約の根拠に使う。 */
export const CustomerDocs = memo(function CustomerDocs({ meetingId, docs }: { meetingId: number; docs: MeetingDoc[] }) {
  const [busy, setBusy] = useState(false);
  const upload = async (files: FileList | null) => {
    if (!files?.length) return;
    setBusy(true);
    try {
      for (const file of Array.from(files)) {
        if (/\.(doc|xls)$/i.test(file.name)) {
          notify(`${file.name}: 旧形式（.doc / .xls）は読めません。docx / xlsx か PDF で保存し直してください。`, "warn");
          continue;
        }
        if (file.size > 8 * 1024 * 1024) {
          notify(`${file.name}: 資料は 8MB までです。`, "warn");
          continue;
        }
        const response = await request(`/api/meetings/${meetingId}/docs`, {
          method: "POST",
          headers: { "Content-Type": "application/octet-stream", "X-File-Name": encodeURIComponent(file.name) },
          raw: file,
        });
        const result = (await response.json()) as { docs: MeetingDoc[]; error: string | null };
        patchDetail((detail) => ({ ...detail, docs: result.docs }));
        notify(result.error ? `${file.name}: 添付しましたが、本文を読めませんでした（${result.error}）。` : `${file.name} を添付しました。会話に合う箇所を自動で探します。`, result.error ? "warn" : "info");
      }
    } catch (err) {
      notify(errorMessage(err), "error");
    } finally {
      setBusy(false);
    }
  };
  const remove = async (doc: MeetingDoc) => {
    try {
      const result = await api<{ docs: MeetingDoc[] }>(`/api/meetings/${meetingId}/docs/${doc.id}`, { method: "DELETE" });
      patchDetail((detail) => ({ ...detail, docs: result.docs }));
    } catch (err) {
      notify(errorMessage(err), "error");
    }
  };
  return (
    <section className="customer-docs" aria-label="顧客資料">
      <h3>顧客資料</h3>
      <p className="muted small">顧客から受け取った資料（PDF・Word・Excel など）を添付すると、会話に合う箇所をこの会議の中で自動で探して出します。</p>
      {docs.length ? (
        <ul>
          {docs.map((doc) => (
            <li key={doc.id}>
              <a className="text-link" href={`/api/meetings/${meetingId}/docs/${doc.id}`} download>
                {doc.name}
              </a>
              <span className="muted small"> {formatBytes(doc.bytes)}</span>
              {doc.error ? <span className="badge" title={doc.error}>本文を読めません</span> : null}
              <button type="button" className="mini" aria-label={`${doc.name} を外す`} title="添付を外す" onClick={() => void remove(doc)}>
                ×
              </button>
            </li>
          ))}
        </ul>
      ) : null}
      <label className="btn small file-btn">
        {busy ? "添付しています…" : "＋ 顧客資料を添付"}
        <input type="file" accept={DOC_ACCEPT} multiple disabled={busy} onChange={(event) => void upload(event.target.files).then(() => (event.target.value = ""))} className="sr-only" />
      </label>
    </section>
  );
});

export const KnowledgePanel = memo(function KnowledgePanel({ detail, done }: { detail: MeetingDetail; done: boolean }) {
  const [query, setQuery] = useState("");
  const [pinOnly, setPinOnly] = useState(false);
  const [busy, setBusy] = useState(false);
  const meetingId = detail.meeting.id;
  const search = async (event: React.FormEvent) => {
    event.preventDefault();
    const q = query.trim();
    if (q.length < 2) {
      notify("2文字以上で検索してください", "warn");
      return;
    }
    setBusy(true);
    try {
      const result = await api<{ hits: Hit[]; warning: string | null }>(`/api/meetings/${meetingId}/knowledge?q=${encodeURIComponent(q)}`);
      await refreshDetail(meetingId);
      setPinOnly(false);
      if (result.warning) notify(result.warning, "warn");
      if (!result.hits.length) notify(`「${q}」に合う資料は見つかりませんでした。`, "info");
    } catch (err) {
      notify(errorMessage(err), "error");
    } finally {
      setBusy(false);
    }
  };
  const list = done ? [...detail.hits.filter((hit) => hit.pinned), ...detail.hits.filter((hit) => !hit.pinned)] : detail.hits;
  const shown = pinOnly ? list.filter((hit) => hit.pinned) : list;
  return (
    <>
      <CustomerDocs meetingId={meetingId} docs={detail.docs || []} />
      <form className="search-row" onSubmit={(event) => void search(event)} role="search">
        <label htmlFor={`k-search-${meetingId}`} className="sr-only">
          社内資料を検索
        </label>
        <input id={`k-search-${meetingId}`} type="search" placeholder="社内資料を検索" value={query} onChange={(event) => setQuery(event.target.value)} />
        <button className="btn small" type="submit" disabled={busy}>
          検索
        </button>
      </form>
      <label className="check small pin-filter">
        <input type="checkbox" checked={pinOnly} onChange={(event) => setPinOnly(event.target.checked)} /> ピン留めのみ
      </label>
      <div className="hits">
        {shown.length ? (
          shown.map((hit) => <HitCard key={hit.id} meetingId={meetingId} hit={hit} />)
        ) : (
          <div className="empty">
            {pinOnly ? (
              "ピン留めした資料はありません。"
            ) : (
              <>
                関連する社内ナレッジはまだありません。会話に合う資料が見つかると自動でここに出ます。資料は
                <a className="text-link" href="#/settings">
                  設定
                </a>
                の「ローカル資料」でアップロードできます（本文は AI に送りません）。
              </>
            )}
          </div>
        )}
      </div>
    </>
  );
});
