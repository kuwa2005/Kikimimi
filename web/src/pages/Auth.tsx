import { useState } from "react";
import { ApiError, errorMessage } from "../api/client";
import { login, register, returnAfterLogin } from "../state/session";
import { Notices } from "../components/Notices";

const LOGIN_ID_RULE = "3〜32 文字。半角の英小文字・数字と _ . - が使えます（大文字は小文字として扱います）。";
const LOST_PASSWORD = "パスワードを忘れた場合、メールでの再設定はありません。このサービスの運用者に再設定を依頼してください。";

function messageFor(err: unknown): string {
  if (err instanceof ApiError) {
    switch (err.code) {
      case "invalid_credentials":
        return "ログインIDかパスワードが違います。";
      case "login_taken":
        return "このログインIDはすでに使われています。別のIDにしてください。";
      case "invalid_login_id":
        return `ログインIDが使えません。${LOGIN_ID_RULE}`;
      case "weak_password":
        return err.message || "パスワードは 8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）。";
      case "rate_limited":
        return `試行が多すぎます。${err.retryAfter ? `${err.retryAfter} 秒ほど` : "しばらく"}待ってからやり直してください。`;
    }
  }
  return errorMessage(err);
}

function AuthFrame({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <main className="auth" id="main">
      <a className="brand auth-brand" href="#/" title="ホームへ" aria-label="会議支援（ホームへ）">
        会議支援<small>録音は解析を待たない</small>
      </a>
      <section className="panel" aria-labelledby="auth-title">
        <h1 id="auth-title">{title}</h1>
        <Notices />
        {children}
      </section>
    </main>
  );
}

export function Login() {
  const [loginId, setLoginId] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setError("");
    setBusy(true);
    try {
      await login(loginId.trim(), password);
      returnAfterLogin();
    } catch (err) {
      setError(messageFor(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <AuthFrame title="ログイン">
      <form className="form" onSubmit={(event) => void submit(event)}>
        <div className="field">
          <label htmlFor="login-id">ログインID</label>
          <input id="login-id" type="text" autoComplete="username" autoCapitalize="none" spellCheck={false} value={loginId} onChange={(event) => setLoginId(event.target.value)} required autoFocus />
        </div>
        <div className="field">
          <label htmlFor="login-password">パスワード</label>
          <input id="login-password" type="password" autoComplete="current-password" value={password} onChange={(event) => setPassword(event.target.value)} required />
        </div>
        {error ? (
          <div className="banner error" role="alert">
            {error}
          </div>
        ) : null}
        <div className="actions">
          <button className="btn primary big" type="submit" disabled={busy}>
            {busy ? "ログインしています…" : "ログイン"}
          </button>
        </div>
      </form>
      <p className="muted small">{LOST_PASSWORD}</p>
      <p>
        アカウントがない場合は <a className="text-link" href="#/register">新しく登録</a>（無料）
      </p>
    </AuthFrame>
  );
}

export function Register() {
  const [loginId, setLoginId] = useState("");
  const [password, setPassword] = useState("");
  const [again, setAgain] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setError("");
    if (password !== again) {
      setError("パスワードが一致しません。");
      return;
    }
    setBusy(true);
    try {
      await register(loginId.trim().toLowerCase(), password);
      location.hash = "#/settings";
    } catch (err) {
      setError(messageFor(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <AuthFrame title="新しく登録">
      <p className="muted">希望するログインIDとパスワードだけで登録できます。メールアドレスやカードは要りません。</p>
      <form className="form" onSubmit={(event) => void submit(event)}>
        <div className="field">
          <label htmlFor="reg-id">ログインID</label>
          <input
            id="reg-id"
            type="text"
            autoComplete="username"
            autoCapitalize="none"
            spellCheck={false}
            value={loginId}
            onChange={(event) => setLoginId(event.target.value)}
            pattern="[A-Za-z0-9_.\-]{3,32}"
            required
            aria-describedby="reg-id-hint"
            autoFocus
          />
          <p className="hint" id="reg-id-hint">
            {LOGIN_ID_RULE}
          </p>
        </div>
        <div className="field">
          <label htmlFor="reg-password">パスワード</label>
          <input id="reg-password" type="password" autoComplete="new-password" minLength={8} maxLength={128} value={password} onChange={(event) => setPassword(event.target.value)} required aria-describedby="reg-password-hint" />
          <p className="hint" id="reg-password-hint">
            8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）。{LOST_PASSWORD}
          </p>
        </div>
        <div className="field">
          <label htmlFor="reg-again">パスワード（確認）</label>
          <input id="reg-again" type="password" autoComplete="new-password" value={again} onChange={(event) => setAgain(event.target.value)} required />
        </div>
        <fieldset className="field">
          <legend>プラン</legend>
          <label className="check">
            <input type="radio" name="plan" defaultChecked />
            Free（登録時に選べるのはこのプランだけです）
          </label>
          <p className="hint">
            このサービスの Free プランと、Gemini / OpenAI の API 料金・利用枠は別です。文字起こしと要約には、登録後に設定する自分の APIキーを使います。キーの契約によっては提供元の料金が発生します。
          </p>
        </fieldset>
        {error ? (
          <div className="banner error" role="alert">
            {error}
          </div>
        ) : null}
        <div className="actions">
          <button className="btn primary big" type="submit" disabled={busy}>
            {busy ? "登録しています…" : "Free で登録"}
          </button>
        </div>
      </form>
      <p>
        登録済みの場合は <a className="text-link" href="#/login">ログイン</a>
      </p>
    </AuthFrame>
  );
}
