import { useStore } from "../lib/store";
import { clearNotice, notices } from "../state/notices";
import { recorderState } from "../recorder/recorder";

// お知らせ（info）・注意（warn）・エラー（error）を色と見出しの両方で分ける。
export function Notices() {
  const list = useStore(notices);
  const rec = useStore(recorderState);
  return (
    <div className="notices" role="status" aria-live="polite">
      {rec.micProblem ? <div className="banner error">{rec.micProblem}</div> : null}
      {list
        .filter((notice) => notice.text !== rec.micProblem)
        .map((notice) => (
          <div key={notice.id} className={`${notice.kind === "info" ? "note" : notice.kind === "error" ? "banner error" : "banner"} dismissible`}>
            <span>{notice.text}</span>
            <button type="button" className="x" aria-label="このお知らせを閉じる" onClick={() => clearNotice(notice.id)}>
              ×
            </button>
          </div>
        ))}
    </div>
  );
}
