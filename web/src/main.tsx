import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { limitText } from "./api/client";
import { App } from "./App";
import { onLimitReached } from "./recorder/recorder";
import { askFinishAfterLimit } from "./state/actions";
import { bootstrap } from "./state/session";
import "./state/theme";
import "./styles.css";

// 上限で録音を止めたら、終了して要約するかを聞く（受け付け済みの録音は残る）。
onLimitReached((meetingId, limit) => {
  void askFinishAfterLimit(meetingId, limitText(limit));
});

void bootstrap();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
