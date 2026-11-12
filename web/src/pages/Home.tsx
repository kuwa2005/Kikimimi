import { useEffect, useState } from "react";
import { localHash, localMeetings } from "../state/offline";
import { session as sessionStore } from "../state/session";
import { useStore as useLocalStore } from "../lib/store";
import { api, errorMessage, request } from "../api/client";
import type { Folder, Meeting, MeetingList, Settings } from "../api/types";
import { fmt, fmtLocal, statusLabel } from "../lib/format";
import { deleteMeeting, keyReady } from "../state/actions";
import { confirmDialog, openDialog } from "../state/dialog";
import { homeFolderFilter, setHomeFolder } from "../state/draft";
import { notify } from "../state/notices";
import { Notices } from "../components/Notices";

const ACTIVE = ["live", "paused", "wrapping"];
const DRAG_TYPE = "application/x-kikimimi-meeting";

type RowProps = {
  meeting: Meeting;
  folders: Folder[];
  sortable: boolean;
  dropBefore: boolean;
  onDeleted: () => void;
  onMove: (meeting: Meeting, folderId: number | null) => void;
  onKeyMove?: (meeting: Meeting, delta: number) => void;
  onDragOverRow?: (meeting: Meeting) => void;
  onDropRow?: (meeting: Meeting, dragged: number) => void;
};

function Row({ meeting, folders, sortable, dropBefore, onDeleted, onMove, onKeyMove, onDragOverRow, onDropRow }: RowProps) {
  return (
    <div
      className={`row ${ACTIVE.includes(meeting.status) ? "row-active" : ""} ${dropBefore ? "drop-before" : ""}`}
      draggable
      onDragStart={(event) => {
        event.dataTransfer.setData(DRAG_TYPE, String(meeting.id));
        event.dataTransfer.effectAllowed = "move";
      }}
      onDragOver={(event) => {
        if (!sortable || !event.dataTransfer.types.includes(DRAG_TYPE)) return;
        event.preventDefault();
        onDragOverRow?.(meeting);
      }}
      onDrop={(event) => {
        if (!sortable) return;
        const dragged = Number(event.dataTransfer.getData(DRAG_TYPE));
        if (!dragged) return;
        event.preventDefault();
        onDropRow?.(meeting, dragged);
      }}
    >
      {sortable ? (
        <button
          type="button"
          className="grip"
          aria-label={`「${meeting.title}」の並び順（↑↓キーで移動）`}
          title="ドラッグで並べ替え（フォルダへドラッグすると移動）"
          onKeyDown={(event) => {
            if (event.key === "ArrowUp" || event.key === "ArrowDown") {
              event.preventDefault();
              onKeyMove?.(meeting, event.key === "ArrowUp" ? -1 : 1);
            }
          }}
        >
          ⋮⋮
        </button>
      ) : null}
      <a className="row-link" href={`#/m/${meeting.id}`} draggable={false}>
        <strong className="row-title">{meeting.title}</strong>
        <span className={`chip ${meeting.status}`}>{statusLabel(meeting.status)}</span>
        <span className="muted">{fmt(meeting.duration_sec)}</span>
        <span className="muted">{fmtLocal(meeting.started_at || meeting.created_at)}</span>
      </a>
      {folders.length ? (
        <select
          className="row-folder compact"
          aria-label={`「${meeting.title}」のフォルダ`}
          value={meeting.folder_id != null ? String(meeting.folder_id) : ""}
          onChange={(event) => onMove(meeting, event.target.value ? Number(event.target.value) : null)}
        >
          <option value="">フォルダなし</option>
          {folders.map((folder) => (
            <option key={folder.id} value={String(folder.id)}>
              {folder.name}
            </option>
          ))}
        </select>
      ) : null}
      <button
        className="row-x"
        type="button"
        aria-label={`「${meeting.title}」を削除`}
        title="削除"
        onClick={() =>
          void deleteMeeting(meeting)
            .then((done) => done && onDeleted())
            .catch((err) => notify(errorMessage(err), "error"))
        }
      >
        ×
      </button>
    </div>
  );
}

async function exportFolder(folder: string, name: string) {
  const values = await openDialog({
    title: `「${name}」をまとめてダウンロード`,
    body: <p className="muted small">フォルダ内の会議（下書きを除く）を、会議ごとの Markdown にして ZIP にまとめます。</p>,
    fields: [
      {
        name: "scope",
        label: "出力範囲",
        type: "select",
        value: "qa",
        options: [
          { value: "summary", label: "要約のみ" },
          { value: "qa", label: "要約＋アジェンダ・メモ" },
          { value: "full", label: "記録一式（全文書き起こしを含む）" },
        ],
      },
    ],
    confirm: "ダウンロード",
  });
  if (!values) return;
  const response = await request(`/api/folders/${folder}/export?scope=${values.scope}`);
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = `${name}.zip`;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}

export function Home() {
  const [list, setList] = useState<MeetingList | null>(null);
  const [folders, setFolders] = useState<Folder[]>([]);
  const [unfiled, setUnfiled] = useState(0);
  const [folder, setFolderState] = useState(() => homeFolderFilter());
  const [keySet, setKeySet] = useState(true);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState("");
  const [reload, setReload] = useState(0);
  const [dropTarget, setDropTarget] = useState<number | null>(null);
  const [folderDrop, setFolderDrop] = useState<string | null>(null);

  const setFolder = (value: string) => {
    setFolderState(value);
    setHomeFolder(value);
  };
  const refresh = () => setReload((value) => value + 1);

  useEffect(() => {
    const onHome = () => refresh();
    window.addEventListener("kikimimi:home", onHome);
    return () => window.removeEventListener("kikimimi:home", onHome);
  }, []);

  useEffect(() => {
    api<Settings>("/api/settings")
      .then((settings) => setKeySet(keyReady(settings)))
      .catch(() => {});
  }, []);

  useEffect(() => {
    api<{ folders: Folder[]; unfiled: number }>("/api/folders")
      .then((data) => {
        setFolders(data.folders);
        setUnfiled(data.unfiled);
        // 消えたフォルダを開いたままにしない。
        if (/^\d+$/.test(folder) && !data.folders.some((item) => String(item.id) === folder)) setFolder("");
      })
      .catch(() => {});
  }, [reload]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const params = new URLSearchParams();
    if (query.trim()) params.set("q", query.trim());
    if (status) params.set("status", status);
    if (folder) params.set("folder", folder);
    const timer = setTimeout(() => {
      api<MeetingList>(`/api/meetings?${params}`)
        .then(setList)
        .catch((err) => notify(errorMessage(err), "error"));
    }, query ? 300 : 0);
    return () => clearTimeout(timer);
  }, [query, status, folder, reload]);

  const filtering = !!(query.trim() || status);
  const meetings = list?.meetings || [];
  const active = filtering || folder ? [] : meetings.filter((meeting) => ACTIVE.includes(meeting.status));
  const rest = filtering || folder ? meetings : meetings.filter((meeting) => !ACTIVE.includes(meeting.status));
  // 検索や状態で絞り込んでいる間は並べ替えない（一部だけ見えている順を変えると分かりにくい）。
  const sortable = !filtering && rest.length > 1;

  const saveOrder = async (ids: number[]) => {
    const byId = new Map(rest.map((meeting) => [meeting.id, meeting]));
    const reordered = ids.map((id) => byId.get(id)!).filter(Boolean);
    setList((current) => (current ? { ...current, meetings: [...active, ...reordered] } : current));
    try {
      await api("/api/meetings/order", { method: "PUT", body: { ids } });
    } catch (err) {
      notify(errorMessage(err), "error");
      refresh();
    }
  };

  const dropOnRow = (target: Meeting, dragged: number) => {
    setDropTarget(null);
    if (target.id === dragged) return;
    const ids = rest.map((meeting) => meeting.id).filter((id) => id !== dragged);
    if (!rest.some((meeting) => meeting.id === dragged)) return;
    ids.splice(ids.indexOf(target.id), 0, dragged);
    void saveOrder(ids);
  };

  const keyMove = (meeting: Meeting, delta: number) => {
    const ids = rest.map((item) => item.id);
    const from = ids.indexOf(meeting.id);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= ids.length) return;
    ids.splice(from, 1);
    ids.splice(to, 0, meeting.id);
    void saveOrder(ids).then(() => {
      setTimeout(() => document.querySelector<HTMLButtonElement>(`[aria-label^="「${CSS.escape(meeting.title)}」の並び順"]`)?.focus(), 0);
    });
  };

  const moveTo = async (meeting: Meeting, folderId: number | null) => {
    try {
      await api(`/api/meetings/${meeting.id}/folder`, { method: "POST", body: { folder_id: folderId } });
      const name = folderId == null ? "フォルダなし" : folders.find((item) => item.id === folderId)?.name || "フォルダ";
      notify(`「${meeting.title}」を「${name}」へ移しました。`, "info");
      refresh();
    } catch (err) {
      notify(errorMessage(err), "error");
    }
  };

  const addFolder = async () => {
    const values = await openDialog({ title: "フォルダを追加", fields: [{ name: "name", label: "フォルダ名", placeholder: "例: A社" }], confirm: "追加" });
    if (!values || !values.name.trim()) return;
    const result = await api<{ id: number; folders: Folder[] }>("/api/folders", { method: "POST", body: { name: values.name } });
    setFolders(result.folders);
    setFolder(String(result.id));
  };

  const renameFolder = async (item: Folder) => {
    const values = await openDialog({ title: "フォルダ名を変更", fields: [{ name: "name", label: "フォルダ名", value: item.name }], confirm: "変更" });
    if (!values || !values.name.trim()) return;
    const result = await api<{ folders: Folder[] }>(`/api/folders/${item.id}`, { method: "PATCH", body: { name: values.name } });
    setFolders(result.folders);
  };

  const removeFolder = async (item: Folder) => {
    const ok = await confirmDialog(
      `フォルダ「${item.name}」を削除しますか？`,
      <p>中の会議（{item.meetings} 件）は削除せず、「フォルダなし」に戻します。</p>,
      "フォルダを削除",
      true,
    );
    if (!ok) return;
    await api(`/api/folders/${item.id}`, { method: "DELETE" });
    setFolder("");
    refresh();
  };

  const folderChip = (value: string, label: string, count: number | null) => (
    <button
      key={value || "all"}
      type="button"
      className={`folder-chip ${folder === value ? "on" : ""} ${folderDrop === value ? "drop" : ""}`}
      aria-pressed={folder === value}
      onClick={() => setFolder(value)}
      onDragOver={(event) => {
        if (value === "" || !event.dataTransfer.types.includes(DRAG_TYPE)) return;
        event.preventDefault();
        setFolderDrop(value);
      }}
      onDragLeave={() => setFolderDrop(null)}
      onDrop={(event) => {
        setFolderDrop(null);
        const dragged = Number(event.dataTransfer.getData(DRAG_TYPE));
        const meeting = meetings.find((item) => item.id === dragged);
        if (!meeting || value === "") return;
        event.preventDefault();
        void moveTo(meeting, value === "none" ? null : Number(value));
      }}
    >
      {label}
      {count != null ? <span className="count">{count}</span> : null}
    </button>
  );

  const current = folders.find((item) => String(item.id) === folder);
  const offline = !!useLocalStore(sessionStore).offline;
  const run = (task: Promise<unknown>) => void task.catch((err) => notify(errorMessage(err), "error"));

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>ホーム</h1>
          {list ? (
            <p className="muted small">
              全 {list.total} 件 ・ 完了 {list.done} 件
            </p>
          ) : null}
        </div>
        <a className="btn primary big" href="#/new">
          ＋ 新しい会議{current ? `（${current.name}）` : ""}
        </a>
      </header>
      <Notices />
      <LocalMeetings />
      {!keySet && !offline ? (
        <div className="banner">
          AI の APIキーは未登録です。Chrome の音声認識なら、キーなしで文字起こしできます。要約や、録音からの確定には
          <a href="#/settings">設定画面</a>でキーを登録してください。
        </div>
      ) : null}
      {list && list.total === 0 ? (
        <section className="panel empty-state">
          <h2>最初の会議を作成しましょう</h2>
          <ol>
            <li>会議のタイトルと「アジェンダ（議題）」を書きます。目安時間も入れられます（空でも始められます）。</li>
            <li>マイクを選んで「会議を開始」を押します。</li>
            <li>話すと書き起こし・回答候補・次に聞くと良い問いかけ・メモが出て、終了すると議事録ができます。</li>
          </ol>
          <a className="btn primary big" href="#/new">
            ＋ 最初の会議を作成
          </a>
        </section>
      ) : (
        <>
          {active.length ? (
            <section className="home-section" aria-labelledby="active-head">
              <h2 id="active-head">進行中</h2>
              <div className="list">
                {active.map((meeting) => (
                  <Row key={meeting.id} meeting={meeting} folders={folders} sortable={false} dropBefore={false} onDeleted={refresh} onMove={(m, f) => void moveTo(m, f)} />
                ))}
              </div>
            </section>
          ) : null}
          <section className="home-section" aria-labelledby="all-head">
            <h2 id="all-head">{filtering ? "検索結果" : "これまでの会議"}</h2>
            <div className="folder-bar" role="group" aria-label="フォルダ">
              {folderChip("", "すべて", null)}
              {folderChip("none", "フォルダなし", unfiled)}
              {folders.map((item) => folderChip(String(item.id), item.name, item.meetings))}
              <button type="button" className="btn small ghost" onClick={() => run(addFolder())}>
                ＋ フォルダ
              </button>
            </div>
            {folder ? (
              <div className="folder-tools">
                <button type="button" className="btn small" onClick={() => run(exportFolder(folder, current ? current.name : "フォルダなし"))}>
                  まとめてダウンロード（ZIP）
                </button>
                {current ? (
                  <>
                    <button type="button" className="btn small" onClick={() => run(renameFolder(current))}>
                      名前を変更
                    </button>
                    <button type="button" className="btn small ghost" onClick={() => run(removeFolder(current))}>
                      フォルダを削除
                    </button>
                  </>
                ) : null}
              </div>
            ) : null}
            <div className="filters" role="search">
              <label htmlFor="home-q" className="sr-only">
                会議を検索
              </label>
              <input id="home-q" type="search" placeholder="タイトル・要約・書き起こし・参加者で検索" value={query} onChange={(event) => setQuery(event.target.value)} />
              <label htmlFor="home-status" className="sr-only">
                状態で絞り込む
              </label>
              <select id="home-status" value={status} onChange={(event) => setStatus(event.target.value)}>
                <option value="">すべて</option>
                <option value="active">進行中・まとめ中</option>
                <option value="draft">下書き</option>
                <option value="done">完了</option>
              </select>
            </div>
            {sortable ? <p className="muted small">⋮⋮ をドラッグして並べ替え、フォルダの上へドラッグすると移せます。</p> : null}
            <div className="list" id="home-list" onDragLeave={() => setDropTarget(null)}>
              {!list ? (
                <div className="empty">読み込み中</div>
              ) : rest.length ? (
                rest.map((meeting) => (
                  <Row
                    key={meeting.id}
                    meeting={meeting}
                    folders={folders}
                    sortable={sortable}
                    dropBefore={dropTarget === meeting.id}
                    onDeleted={refresh}
                    onMove={(m, f) => void moveTo(m, f)}
                    onKeyMove={keyMove}
                    onDragOverRow={(target) => setDropTarget(target.id)}
                    onDropRow={dropOnRow}
                  />
                ))
              ) : (
                <div className="empty">{filtering ? "条件に合う会議はありません。" : folder ? "このフォルダに会議はありません。" : "過去の会議はまだありません。"}</div>
              )}
            </div>
          </section>
        </>
      )}
    </div>
  );
}

/** この端末にだけある（まだサーバへ引き継いでいない）会議。オフラインの間も開ける。 */
function LocalMeetings() {
  const list = useLocalStore(localMeetings);
  const offline = !!useLocalStore(sessionStore).offline;
  if (!list.length && !offline) return null;
  return (
    <section className="panel" aria-labelledby="local-head">
      <h2 id="local-head">この端末に保存している会議</h2>
      {offline ? <p className="muted small">サーバにつながっていません。「＋ 新しい会議」から、オフラインのまま録音を始められます。つながると自動で送ります。</p> : null}
      {list.length ? (
        <div className="local-list">
          {list.map((m) => (
            <div key={m.localId} className="row">
              <a href={localHash(m.localId)}>{m.title}</a>
              <span className="muted small">{m.finishClose != null ? "終了・送信待ち" : m.serverId ? "引き継ぎ中" : "サーバ未登録"}</span>
            </div>
          ))}
        </div>
      ) : null}
    </section>
  );
}
