import type { UsageItem } from "./types";

// 認証は Cookie。GET 以外は CSRF トークンをヘッダで送る。

export class ApiError extends Error {
  status: number;
  code: string;
  limit?: UsageItem;
  retryAfter?: number;

  constructor(message: string, status: number, code = "", limit?: UsageItem, retryAfter?: number) {
    super(message);
    this.status = status;
    this.code = code;
    this.limit = limit;
    this.retryAfter = retryAfter;
  }
}

let csrf = "";
let unauthenticated: () => void = () => {};

export function setCsrf(token: string) {
  csrf = token;
}

export function csrfToken() {
  return csrf;
}

/** セッションが切れたときの処理（ログイン画面へ）を登録する。 */
export function onUnauthenticated(handler: () => void) {
  unauthenticated = handler;
}

export type RequestOptions = {
  method?: string;
  body?: unknown;
  raw?: BodyInit;
  headers?: Record<string, string>;
  signal?: AbortSignal;
};

/** 応答をそのまま返す。エラーは ApiError にして投げる。 */
export async function request(path: string, options: RequestOptions = {}): Promise<Response> {
  const method = options.method || "GET";
  const headers: Record<string, string> = Object.assign({}, options.headers);
  let body: BodyInit | undefined = options.raw;
  if (options.body !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(options.body);
  }
  if (method !== "GET" && csrf) headers["X-CSRF-Token"] = csrf;
  let response: Response;
  try {
    response = await fetch(path, { method, headers, body, signal: options.signal });
  } catch (err) {
    if (err instanceof DOMException && err.name === "AbortError") throw err;
    // 回線が無い・サーバに届かない。status 0 の ApiError にして、呼び出し側が見分けられるようにする。
    throw new ApiError("サーバに接続できません（オフラインか、回線が不安定です）", 0, "network");
  }
  if (!response.ok) throw await toError(response);
  return response;
}

/** 回線やサーバに届かなかった失敗か（送り直せば通る見込みがある）。 */
export function isNetworkError(err: unknown): boolean {
  return err instanceof ApiError && err.code === "network";
}

export async function toError(response: Response): Promise<ApiError> {
  const type = response.headers.get("content-type") || "";
  let message = response.statusText || `HTTP ${response.status}`;
  let code = "";
  let limit: UsageItem | undefined;
  let retryAfter: number | undefined;
  if (type.includes("json")) {
    const data = await response.json().catch(() => ({}));
    message = data.error || message;
    code = data.code || "";
    limit = data.limit;
    retryAfter = data.retry_after;
  }
  if (response.status === 401 && code !== "invalid_credentials") unauthenticated();
  return new ApiError(message, response.status, code, limit, retryAfter);
}

export async function api<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const response = await request(path, options);
  if (response.status === 204) return undefined as T;
  const type = response.headers.get("content-type") || "";
  if (type.includes("json")) return (await response.json()) as T;
  return (await response.text()) as unknown as T;
}

/** 上限エラーを、利用者に見せる一文にする。 */
export function limitText(limit: UsageItem): string {
  const fmt = (value: number | null) => (value === null ? "無制限" : formatAmount(value, limit.unit));
  const used = limit.used === null ? "" : `（現在 ${fmt(limit.used)}）`;
  return `${limit.label}の上限 ${fmt(limit.limit)} に達しました${used}。`;
}

export function formatAmount(value: number, unit: string): string {
  if (unit === "バイト") return formatBytes(value);
  if (unit === "分") return `${Math.round(value * 10) / 10} 分`;
  return `${value} ${unit}`;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[index]}`;
}

export function errorMessage(err: unknown): string {
  if (err instanceof ApiError && err.code === "limit_exceeded" && err.limit) return limitText(err.limit);
  if (err instanceof Error) return err.message;
  return String(err);
}
