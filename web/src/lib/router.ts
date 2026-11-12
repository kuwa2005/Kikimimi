import { useSyncExternalStore } from "react";

// ハッシュでの画面遷移。#/、#/new、#/m/:id、#/settings、#/account、#/login、#/register、#/reset
export type Route =
  | { name: "home" }
  | { name: "new" }
  | { name: "meeting"; id: number }
  | { name: "local"; id: number }
  | { name: "settings" }
  | { name: "account" }
  | { name: "login" }
  | { name: "register" }
  | { name: "reset" }
  | { name: "unknown" };

export function parseHash(hash: string): Route {
  const value = hash || "#/";
  if (value === "#/" || value === "#" || value === "") return { name: "home" };
  if (value === "#/new") return { name: "new" };
  if (value === "#/settings") return { name: "settings" };
  if (value === "#/account") return { name: "account" };
  if (value === "#/login") return { name: "login" };
  if (value === "#/register") return { name: "register" };
  if (value === "#/reset") return { name: "reset" };
  const meeting = value.match(/^#\/m\/(\d+)$/);
  if (meeting) return { name: "meeting", id: Number(meeting[1]) };
  // オフラインで始めた会議（端末内の番号は負の数。画面の番号は正の数で持つ）。
  const local = value.match(/^#\/local\/(\d+)$/);
  if (local) return { name: "local", id: -Number(local[1]) };
  return { name: "unknown" };
}

function subscribe(listener: () => void) {
  window.addEventListener("hashchange", listener);
  return () => window.removeEventListener("hashchange", listener);
}

export function useHash(): string {
  return useSyncExternalStore(subscribe, () => location.hash || "#/", () => "#/");
}

export function navigate(hash: string) {
  if (location.hash === hash) return;
  location.hash = hash;
}
