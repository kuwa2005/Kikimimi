import type { ReactNode } from "react";
import type { Route } from "../lib/router";
import { useStore } from "../lib/store";
import { recorderState } from "../recorder/recorder";
import { session } from "../state/session";
import { localHash } from "../state/offline";
import { setTheme, theme, THEME_LABELS, type ThemeChoice } from "../state/theme";
import { logout } from "../state/actions";
import { notify } from "../state/notices";

const NAV: { hash: string; label: string; match: Route["name"] }[] = [
  { hash: "#/", label: "ホーム", match: "home" },
  { hash: "#/new", label: "新しい会議", match: "new" },
  { hash: "#/settings", label: "設定", match: "settings" },
  { hash: "#/account", label: "アカウント", match: "account" },
];

export function Shell({ route, children, onCommand }: { route: Route; children: ReactNode; onCommand: () => void }) {
  const rec = useStore(recorderState);
  const { user, offline } = useStore(session);
  const unsent = rec.queueLength + rec.parked + rec.captionsPending;
  const choice = useStore(theme);
  const mac = /Mac|iPhone|iPad/.test(navigator.platform);
  return (
    <div className="app">
      <aside className="side" aria-label="メニュー">
        <a
          className="brand"
          href="#/"
          title="ホームへ"
          aria-label="会議支援（ホームへ）"
          onClick={() => {
            // ホームを開いているときに押したら、一覧を読み直して先頭へ戻る。
            if (location.hash === "#/" || location.hash === "") {
              window.dispatchEvent(new Event("kikimimi:home"));
              document.getElementById("main")?.scrollTo({ top: 0 });
              window.scrollTo({ top: 0 });
            }
          }}
        >
          会議支援<small>録音は解析を待たない</small>
        </a>
        <button type="button" className="find" onClick={onCommand} aria-keyshortcuts={mac ? "Meta+K" : "Control+K"}>
          <span>操作を探す</span>
          <kbd>{mac ? "⌘K" : "Ctrl+K"}</kbd>
        </button>
        <nav className="nav" aria-label="画面">
          {NAV.map((item) => (
            <a key={item.hash} href={item.hash} aria-current={route.name === item.match ? "page" : undefined}>
              {item.label}
            </a>
          ))}
        </nav>
        {rec.capturing && rec.meetingId != null ? (
          <a className="rec-pill" href={rec.meetingId < 0 ? localHash(rec.meetingId) : `#/m/${rec.meetingId}`}>
            <span className="dot" aria-hidden="true" />
            <span>録音中の会議へ戻る</span>
          </a>
        ) : null}
        {offline || unsent ? (
          <p className="muted small" role="status" aria-live="polite">
            {offline ? <span className="offline-badge">オフライン</span> : null}
            {unsent ? ` 端末に保存中の未送信 ${unsent} 件（自動で送ります）` : offline ? " 録音はこの端末に保存します" : ""}
          </p>
        ) : null}
        <div className="side-foot">
          {user ? (
            <div className="who">
              <span className="who-id" title="ログイン中のID">
                {user.login_id}
              </span>
              <span className="chip plan">{user.plan.name}</span>
              <button type="button" className="link-btn" onClick={() => void logout().catch((err) => notify(err.message, "error"))}>
                ログアウト
              </button>
            </div>
          ) : null}
          <label className="theme-pick">
            <span>表示</span>
            <select value={choice} onChange={(event) => setTheme(event.target.value as ThemeChoice)} aria-label="表示のテーマ">
              {(Object.keys(THEME_LABELS) as ThemeChoice[]).map((key) => (
                <option key={key} value={key}>
                  {THEME_LABELS[key]}
                </option>
              ))}
            </select>
          </label>
        </div>
      </aside>
      <main className="main" id="main">
        {children}
      </main>
    </div>
  );
}
