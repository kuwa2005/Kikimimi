import { createStore } from "../lib/store";

// 画面上部のお知らせ。info は数秒で消え、warn / error は閉じるまで残る。
export type NoticeKind = "info" | "warn" | "error";
export type Notice = { id: string; text: string; kind: NoticeKind; at: number };

export const notices = createStore<Notice[]>([]);

/** 録音と送信に関わるお知らせと、設定への案内（setup）。画面を移っても持ち越す。 */
const STICKY = ["upload", "upload-drop", "mic", "tab", "limit", "auth", "setup"];

export function notify(text: string, kind: NoticeKind = "warn", key = "") {
  if (!text) return;
  const id = key || text;
  notices.set((list) => {
    const next = list.filter((notice) => notice.id !== id);
    next.push({ id, text, kind, at: Date.now() });
    return next.slice(-4);
  });
}

export function clearNotice(key: string) {
  notices.set((list) => (list.some((notice) => notice.id === key) ? list.filter((notice) => notice.id !== key) : list));
}

export function clearForRoute() {
  notices.set((list) => list.filter((notice) => STICKY.includes(notice.id)));
}

export function clearAllNotices() {
  notices.set([]);
}

setInterval(() => {
  const now = Date.now();
  notices.set((list) => {
    const next = list.filter((notice) => notice.kind !== "info" || now - notice.at < 6000);
    return next.length === list.length ? list : next;
  });
}, 500);
