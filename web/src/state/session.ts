import { api, onUnauthenticated, setCsrf } from "../api/client";
import type { AuthResponse, User } from "../api/types";
import { createStore } from "../lib/store";
import { sessionGet, sessionRemove, sessionSet } from "../lib/prefs";
import { navigate } from "../lib/router";
import { notify } from "./notices";
import { resumeForUser } from "../recorder/recorder";

export type Session = { status: "loading" | "anon" | "user"; user: User | null };

export const session = createStore<Session>({ status: "loading", user: null });

const RETURN_TO = "kikimimi.returnTo";

export function currentUser(): User | null {
  return session.get().user;
}

function enter(response: AuthResponse) {
  setCsrf(response.csrf);
  session.set({ status: "user", user: response.user });
  resumeForUser(response.user.id);
}

export async function bootstrap() {
  try {
    enter(await api<AuthResponse>("/api/auth/me"));
  } catch {
    session.set({ status: "anon", user: null });
  }
}

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
  session.set({ status: "anon", user: null });
}
