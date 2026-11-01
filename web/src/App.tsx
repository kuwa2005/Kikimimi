import { useEffect, useMemo, useState } from "react";
import { errorMessage } from "./api/client";
import { navigate, parseHash, useHash, type Route } from "./lib/router";
import { useStore } from "./lib/store";
import { recorderState } from "./recorder/recorder";
import { finishMeeting, logout, pauseMeeting, resumeMeeting } from "./state/actions";
import { guarded } from "./state/busy";
import { meetingStore } from "./state/meeting";
import { clearForRoute, notify } from "./state/notices";
import { rememberReturn, session } from "./state/session";
import { setTheme, theme, THEME_LABELS, type ThemeChoice } from "./state/theme";
import { CommandMenu, type Command } from "./components/CommandMenu";
import { DialogHost } from "./components/DialogHost";
import { Shell } from "./components/Shell";
import { Account } from "./pages/Account";
import { Login, Register } from "./pages/Auth";
import { Home } from "./pages/Home";
import { MeetingForm } from "./pages/MeetingForm";
import { MeetingPage } from "./pages/Meeting";
import { Settings } from "./pages/Settings";

const PUBLIC: Route["name"][] = ["login", "register"];

function useCommands(route: Route): Command[] {
  const rec = useStore(recorderState);
  const meetingState = useStore(meetingStore);
  const choice = useStore(theme);
  return useMemo(() => {
    const list: Command[] = [
      { id: "new", label: "新しい会議", keywords: "作成 開始 new", run: () => navigate("#/new") },
      { id: "home", label: "ホームを開く", keywords: "一覧 会議 home", run: () => navigate("#/") },
      { id: "settings", label: "設定を開く", keywords: "APIキー 資料 テンプレート settings", run: () => navigate("#/settings") },
      { id: "account", label: "アカウントを開く", keywords: "プラン 利用量 パスワード account", run: () => navigate("#/account") },
    ];
    if (rec.capturing && rec.meetingId != null) {
      list.unshift({ id: "back", label: "録音中の会議へ戻る", keywords: "録音中 recording", run: () => navigate(`#/m/${rec.meetingId}`) });
    }
    const detail = route.name === "meeting" && meetingState.detail?.meeting.id === route.id ? meetingState.detail : null;
    if (detail && detail.meeting.status !== "done" && detail.meeting.status !== "draft" && !detail.meeting.finish_requested) {
      const id = detail.meeting.id;
      if (rec.capturing && rec.meetingId === id) {
        list.push({ id: "pause", label: "この会議を一時停止", keywords: "pause 止める", run: () => guarded("pause", () => pauseMeeting(id)) });
      } else if (detail.meeting.status !== "wrapping") {
        list.push({ id: "resume", label: "この会議の録音を再開", keywords: "resume 再開", run: () => guarded("resume", () => resumeMeeting(id)) });
      }
      // 終了は確認ダイアログを必ず通す。
      list.push({ id: "finish", label: "この会議を終了…", hint: "確認してから終了します", keywords: "finish 終わる", run: () => guarded("finish", () => finishMeeting(id)) });
    }
    (Object.keys(THEME_LABELS) as ThemeChoice[])
      .filter((key) => key !== choice)
      .forEach((key) => list.push({ id: `theme-${key}`, label: `表示を「${THEME_LABELS[key]}」にする`, keywords: "テーマ ダーク ライト theme", run: () => setTheme(key) }));
    list.push({ id: "logout", label: "ログアウト…", keywords: "logout サインアウト", run: () => void logout().catch((err) => notify(errorMessage(err), "error")) });
    return list;
  }, [rec.capturing, rec.meetingId, route, meetingState.detail, choice]);
}

export function App() {
  const hash = useHash();
  const route = parseHash(hash);
  const auth = useStore(session);
  const [menu, setMenu] = useState(false);
  const commands = useCommands(route);

  useEffect(() => {
    clearForRoute();
  }, [hash]);

  // ログインしていなければ、公開画面（ログイン・登録）以外を開かせない。
  useEffect(() => {
    if (auth.status === "anon" && !PUBLIC.includes(route.name)) {
      rememberReturn(location.hash);
      navigate("#/login");
    }
    if (auth.status === "user" && PUBLIC.includes(route.name)) navigate("#/");
    if (route.name === "unknown") navigate("#/");
  }, [auth.status, route.name]);

  // Ctrl+K / Cmd+K。日本語の変換中のキーは無視する。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.isComposing || event.keyCode === 229) return;
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k" && session.get().status === "user") {
        event.preventDefault();
        setMenu(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (auth.status === "loading") {
    return <p className="muted splash">読み込み中</p>;
  }
  if (auth.status === "anon") {
    return (
      <>
        {route.name === "register" ? <Register /> : <Login />}
        <DialogHost />
      </>
    );
  }

  let page;
  switch (route.name) {
    case "new":
      page = <MeetingForm key="new" detail={null} />;
      break;
    case "meeting":
      page = <MeetingPage id={route.id} />;
      break;
    case "settings":
      page = <Settings />;
      break;
    case "account":
      page = <Account />;
      break;
    default:
      page = <Home />;
  }
  return (
    <>
      <Shell route={route} onCommand={() => setMenu(true)}>
        {page}
      </Shell>
      {menu ? <CommandMenu commands={commands} onClose={() => setMenu(false)} /> : null}
      <DialogHost />
    </>
  );
}
