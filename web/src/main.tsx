import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { limitText } from "./api/client";
import { App } from "./App";
import { onLimitReached } from "./recorder/recorder";
import { askFinishAfterLimit } from "./state/actions";
import { bootstrap, refreshSession } from "./state/session";
import { setSessionRefresher } from "./recorder/recorder";
import "./state/offline";
import "./state/theme";
import "./styles.css";

// 上限で録音を止めたら、終了して要約するかを聞く（受け付け済みの録音は残る）。
onLimitReached((meetingId, limit) => {
  void askFinishAfterLimit(meetingId, limitText(limit));
});

setSessionRefresher(refreshSession);
void bootstrap();

// 画面を端末にキャッシュし、回線が無くても開いて録音を始められるようにする。
if ("serviceWorker" in navigator && window.isSecureContext) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
  });
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
