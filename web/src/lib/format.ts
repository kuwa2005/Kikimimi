import type { MeetingStatus } from "../api/types";

export function fmt(sec: number | null | undefined): string {
  const whole = Math.max(0, Math.floor(sec || 0));
  const hours = Math.floor(whole / 3600);
  const min = Math.floor((whole % 3600) / 60);
  const rem = whole % 60;
  const mmss = String(min).padStart(2, "0") + ":" + String(rem).padStart(2, "0");
  return hours ? `${hours}:${mmss}` : mmss;
}

// 保存している UTC の時刻を、この端末の時刻で表示する。
export function fmtLocal(iso: string | null | undefined, withTime = true): string {
  if (!iso) return "";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const options: Intl.DateTimeFormatOptions = { year: "numeric", month: "2-digit", day: "2-digit" };
  if (withTime) Object.assign(options, { hour: "2-digit", minute: "2-digit" });
  return date.toLocaleString("ja-JP", options);
}

export function statusLabel(status: MeetingStatus): string {
  return { draft: "下書き", live: "進行中", paused: "一時停止中", wrapping: "まとめ中", done: "完了" }[status] || status;
}

export function nowTitle(): string {
  const now = new Date();
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} ${pad(now.getHours())}:${pad(now.getMinutes())}:${pad(now.getSeconds())}`;
}

/** 長い URL・パスを、出典として読める短い形にする。 */
export function shortLocator(locator: string): string {
  if (/^https?:\/\//i.test(locator)) {
    try {
      const url = new URL(locator);
      const tail = url.pathname.split("/").filter(Boolean).pop() || "";
      return tail ? `${url.hostname} … ${decodeURIComponent(tail)}` : url.hostname;
    } catch {
      return locator;
    }
  }
  const parts = locator.split(/[\\/]/).filter(Boolean);
  return parts.pop() || locator;
}

export function isUrl(locator: string): boolean {
  return /^https?:\/\//i.test(locator);
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
