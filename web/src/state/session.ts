import { api, isNetworkError, onUnauthenticated, setCsrf } from "../api/client";
import type { AuthResponse, User } from "../api/types";
import { createStore } from "../lib/store";
import { sessionGet, sessionRemove, sessionSet } from "../lib/prefs";
import { navigate } from "../lib/router";
import { notify } from "./notices";
import { recorderState, resumeForUser } from "../recorder/recorder";

/** offline: サーバに届かないため、この端末で最後にログインした利用者として動いている（録音だけできる）。 */
export type Session = { status: "loading" | "anon" | "user"; user: User | null; offline?: boolean };

export const session = createStore<Session>({ status: "loading", user: null });

// 回線が無いときでも録音を始められるよう、最後にログインした利用者を覚える（パスワードやトークンは残さない）。
const LAST_USER = "kikimimi.lastUser";

function rememberUser(user: User | null) {
  try {
    if (user) localStorage.setItem(LAST_USER, JSON.stringify(user));
    else localStorage.removeItem(LAST_USER);
  } catch {
    // 保存できなくても動く
  }
}

function lastUser(): User | null {
  try {
    const raw = localStorage.getItem(LAST_USER);
    return raw ? (JSON.parse(raw) as User) : null;
  } catch {
    return null;
  }
}

export function isOffline(): boolean {
  return !!session.get().offline;
}

const RETURN_TO = "kikimimi.returnTo";

export function currentUser(): User | null {
  return session.get().user;
}

function enter(response: AuthResponse) {
  const wasOffline = isOffline();
  setCsrf(response.csrf);
  rememberUser(response.user);
  session.set({ status: "user", user: response.user, offline: false });
  if (wasOffline) notify("サーバにつながりました。オフラインの間の録音を送ります。", "info", "net");
  resumeForUser(response.user.id);
}

export async function bootstrap() {
  try {
    enter(await api<AuthResponse>("/api/auth/me"));
  } catch (err) {
    const user = lastUser();
    if (isNetworkError(err) && user) {
      // サーバに届かない。最後にログインした利用者として、録音だけはできるようにする。
      session.set({ status: "user", user, offline: true });
      notify("サーバにつながっていません。録音はこの端末に保存し、つながったら自動で送ります。", "warn", "net");
      resumeForUser(user.id, true);
    } else {
      session.set({ status: "anon", user: null });
    }
  }
}

/** オフラインで動いているとき、サーバに届くか確かめ、届けば通常のログイン状態に戻す。 */
export async function refreshSession(): Promise<boolean> {
  if (!isOffline()) return true;
  try {
    enter(await api<AuthResponse>("/api/auth/me"));
    return true;
  } catch (err) {
    if (!isNetworkError(err)) {
      // ログインが切れていた。録音中は画面を変えずに録音を続け、止めたあとでログイン画面へ。
      if (recorderState.get().capturing) {
        notify("サーバにはつながりましたが、ログインの有効期限が切れています。録音は続けています（この端末に保存）。一時停止か終了のあとでログインし直すと送ります。", "warn", "auth");
        return false;
      }
      session.set({ status: "anon", user: null, offline: false });
      notify("ログインし直すと、端末に保存した録音を送ります。", "warn", "auth");
      navigate("#/login");
    }
    return false;
  }
}

window.addEventListener("online", () => void refreshSession());
setInterval(() => void refreshSession(), 10000);

/** セッションが切れたら、今の画面を覚えてログイン画面へ。録音中の区間は送らずに待たせる。 */
onUnauthenticated(() => {
  // 起動時の確認（未ログイン）は「切れた」ではないので、知らせない。
  if (session.get().status !== "user") return;
  const here = location.hash || "#/";
  if (here !== "#/login" && here !== "#/register" && here !== "#/reset") sessionSet(RETURN_TO, here);
  setCsrf("");
  session.set({ status: "anon", user: null });
  notify("ログインの有効期限が切れました。もう一度ログインしてください。", "warn", "auth");
  navigate("#/login");
});

/** ログイン前に開こうとした画面を覚える。 */
export function rememberReturn(hash: string) {
  if (hash && hash !== "#/" && hash !== "#/login" && hash !== "#/register" && hash !== "#/reset") sessionSet(RETURN_TO, hash);
}

export function returnAfterLogin() {
  const target = sessionGet<string>(RETURN_TO);
  sessionRemove(RETURN_TO);
  navigate(target && target !== "#/login" && target !== "#/register" ? target : "#/");
}

export async function login(loginId: string, password: string) {
  enter(await api<AuthResponse>("/api/auth/login", { method: "POST", body: { login_id: loginId, password } }));
}

export async function requestRegisterCode(loginId: string, invite: string) {
  await api<{ sent: boolean }>("/api/auth/register/code", { method: "POST", body: { login_id: loginId, invite } });
}

export async function verifyRegisterCode(loginId: string, code: string) {
  await api<{ ok: boolean }>("/api/auth/register/verify", { method: "POST", body: { login_id: loginId, code } });
}

export async function register(loginId: string, password: string, code: string, invite: string) {
  // プランは送らない。登録時はサーバが必ず Free を割り当てる。
  enter(await api<AuthResponse>("/api/auth/register", { method: "POST", body: { login_id: loginId, password, code, invite } }));
}

export async function requestPasswordReset(loginId: string) {
  await api<{ sent: boolean }>("/api/auth/password/forgot", { method: "POST", body: { login_id: loginId } });
}

export async function resetPassword(loginId: string, code: string, password: string) {
  enter(await api<AuthResponse>("/api/auth/password/reset", { method: "POST", body: { login_id: loginId, code, password } }));
}

export async function changePassword(current: string, next: string) {
  await api<void>("/api/auth/password", { method: "POST", body: { current_password: current, new_password: next } });
}

export function leaveSession() {
  setCsrf("");
  rememberUser(null);
  session.set({ status: "anon", user: null, offline: false });
}
