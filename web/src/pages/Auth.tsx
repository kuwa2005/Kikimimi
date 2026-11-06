import { useEffect, useState } from "react";
import { ApiError, errorMessage } from "../api/client";
import { login, register, requestPasswordReset, requestRegisterCode, resetPassword, returnAfterLogin, verifyRegisterCode } from "../state/session";
import { Notices } from "../components/Notices";

const LOGIN_ID_RULE = "メールアドレスを入力してください。大文字は小文字として扱います。";
const PASSWORD_RULE = "8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）。";

function messageFor(err: unknown): string {
  if (err instanceof ApiError) {
    switch (err.code) {
      case "invalid_credentials":
        return "ログインIDかパスワードが違います。";
      case "login_taken":
        return "このメールアドレスはすでに登録されています。ログインしてください。";
      case "invalid_login_id":
        return `ログインID（メールアドレス）の形になっていません。${LOGIN_ID_RULE}`;
      case "invalid_invite":
        return "招待コードが違います。";
      case "invalid_code":
        return "確認コードが違います。メールに届いた6桁の数字を入力してください。";
      case "code_expired":
        return "確認コードの期限が切れました。もう一度メールを送ってください。";
      case "code_locked":
        return "確認コードを何度も間違えました。もう一度メールを送ってください。";
      case "password_changed":
        return "パスワードが、確認コードを送ったときと違います。最初からやり直してください。";
      case "mail_unconfigured":
        return "確認メールを送る設定がありません。運用者に連絡してください。";
      case "mail_failed":
        return "確認メールを送れませんでした。しばらくしてからやり直してください。";
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
          <label htmlFor="login-id">ログインID（メールアドレス）</label>
          <input id="login-id" type="text" inputMode="email" autoComplete="username" autoCapitalize="none" spellCheck={false} value={loginId} onChange={(event) => setLoginId(event.target.value)} required autoFocus aria-describedby="login-id-hint" />
          <p className="hint" id="login-id-hint">登録したメールアドレスです。</p>
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
      <p className="muted small">
        パスワードを忘れた場合は <a className="text-link" href="#/reset">再設定用のメール</a> を送れます。
      </p>
      <p>
        アカウントがない場合は <a className="text-link" href="#/register">新しく登録</a>（無料）
      </p>
    </AuthFrame>
  );
}

function DigitCode({ id, value, onChange, hint }: { id: string; value: string; onChange: (value: string) => void; hint: string }) {
  return (
    <div className="field">
      <label htmlFor={id}>確認コード</label>
      <input
        id={id}
        className="code-input"
        type="text"
        inputMode="numeric"
        autoComplete="one-time-code"
        maxLength={6}
        value={value}
        onChange={(event) => onChange(event.target.value.replace(/\D/g, "").slice(0, 6))}
        aria-describedby={`${id}-hint`}
        autoFocus
      />
      <p className="hint" id={`${id}-hint`}>{hint}</p>
    </div>
  );
}

function Alert({ error }: { error: string }) {
  if (!error) return null;
  return (
    <div className="banner error" role="alert">
      {error}
    </div>
  );
}

const TERMS = [
  "このサービスは、会議の録音、文字起こし、議事録を、登録した会員ごとに保存します。",
  "ログインIDは、登録したメールアドレスです。登録できるプランは Free だけです。",
  "録音と議事録は、その会員のアカウントに残ります。",
  "文字起こしと要約に使う API キーは会員自身のものです。提供元の料金は会員の負担です。",
  "パスワードを忘れた場合は、登録したメールアドレスへ再設定用の確認コードを送ります。",
];

export function Register() {
  const [loginId, setLoginId] = useState("");
  const [invite, setInvite] = useState("");
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [again, setAgain] = useState("");
  const [sentTo, setSentTo] = useState("");
  const [step, setStep] = useState<"mail" | "code" | "password" | "terms">("mail");
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const email = loginId.trim().toLowerCase();

  useEffect(() => {
    if (step !== "code" || code.length !== 6) return;
    let cancel = false;
    setBusy(true);
    setError("");
    verifyRegisterCode(sentTo, code)
      .then(() => {
        if (!cancel) setStep("password");
      })
      .catch((err: unknown) => {
        if (!cancel) setError(messageFor(err));
      })
      .finally(() => {
        if (!cancel) setBusy(false);
      });
    return () => {
      cancel = true;
    };
  }, [code, step, sentTo]);

  const sendCode = async () => {
    setError("");
    setNotice("");
    setBusy(true);
    try {
      await requestRegisterCode(email, invite.trim());
      setSentTo(email);
      setCode("");
      setStep("code");
      setNotice(`${email} に6桁の確認コードを送りました。`);
    } catch (err) {
      setError(messageFor(err));
    } finally {
      setBusy(false);
    }
  };

  const toTerms = (event: React.FormEvent) => {
    event.preventDefault();
    setError("");
    if (password !== again) {
      setError("パスワードが一致しません。");
      return;
    }
    setStep("terms");
  };

  const finish = async () => {
    setError("");
    setBusy(true);
    try {
      await register(sentTo, password, code, invite.trim());
      location.hash = "#/settings";
    } catch (err) {
      setError(messageFor(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthFrame title="新しく登録">
      <p className="muted">
        {step === "mail" && "メールアドレスと招待コードを入力してください。"}
        {step === "code" && "届いた6桁の数字を入力してください。そろうまで待ちます。"}
        {step === "password" && "ログインに使うパスワードを決めてください。"}
        {step === "terms" && "利用規約を確認し、同意すると登録が完了します。"}
      </p>
      <form
        className="form"
        onSubmit={(event) => {
          event.preventDefault();
          if (step === "mail") void sendCode();
          if (step === "password") toTerms(event);
        }}
      >
        {step === "mail" ? (
          <>
            <div className="field">
              <label htmlFor="reg-id">ログインID（メールアドレス）</label>
              <input id="reg-id" type="email" autoComplete="email" autoCapitalize="none" spellCheck={false} value={loginId} onChange={(event) => setLoginId(event.target.value)} required aria-describedby="reg-id-hint" autoFocus />
              <p className="hint" id="reg-id-hint">{LOGIN_ID_RULE}</p>
            </div>
            <div className="field">
              <label htmlFor="reg-invite">招待コード</label>
              <input id="reg-invite" type="text" autoComplete="off" autoCapitalize="none" spellCheck={false} value={invite} onChange={(event) => setInvite(event.target.value)} required aria-describedby="reg-invite-hint" />
              <p className="hint" id="reg-invite-hint">招待コードがないと登録できません。</p>
            </div>
          </>
        ) : null}
        {step === "code" ? (
          <DigitCode id="reg-code" value={code} onChange={setCode} hint={`${sentTo} に届いた6桁の数字です。10分間有効です。6桁そろうと次へ進みます。`} />
        ) : null}
        {step === "password" ? (
          <>
            <div className="field">
              <label htmlFor="reg-password">パスワード</label>
              <input id="reg-password" type="password" autoComplete="new-password" minLength={8} maxLength={128} value={password} onChange={(event) => setPassword(event.target.value)} required aria-describedby="reg-password-hint" autoFocus />
              <p className="hint" id="reg-password-hint">{PASSWORD_RULE}</p>
            </div>
            <div className="field">
              <label htmlFor="reg-again">パスワード（確認）</label>
              <input id="reg-again" type="password" autoComplete="new-password" value={again} onChange={(event) => setAgain(event.target.value)} required />
            </div>
          </>
        ) : null}
        {step === "terms" ? (
          <div className="terms" tabIndex={0} aria-label="利用規約">
            <h2>利用規約</h2>
            <ul>
              {TERMS.map((line) => (
                <li key={line}>{line}</li>
              ))}
            </ul>
          </div>
        ) : null}
        {notice ? <div className="note">{notice}</div> : null}
        <Alert error={error} />
        <div className="actions">
          {step === "mail" ? (
            <button className="btn primary big" type="submit" disabled={busy}>
              {busy ? "送っています…" : "確認コードを送る"}
            </button>
          ) : null}
          {step === "code" ? (
            <>
              <button className="btn" type="button" disabled={busy} onClick={() => void sendCode()}>確認コードを再送</button>
              <button className="btn" type="button" disabled={busy} onClick={() => { setStep("mail"); setSentTo(""); setCode(""); setNotice(""); setError(""); }}>メールアドレスを変更</button>
            </>
          ) : null}
          {step === "password" ? (
            <button className="btn primary big" type="submit" disabled={busy}>次へ</button>
          ) : null}
          {step === "terms" ? (
            <>
              <button className="btn primary big" type="button" disabled={busy} onClick={() => void finish()}>{busy ? "登録しています…" : "同意して登録"}</button>
              <button className="btn" type="button" disabled={busy} onClick={() => setStep("password")}>戻る</button>
            </>
          ) : null}
        </div>
      </form>
      <p>
        登録済みの場合は <a className="text-link" href="#/login">ログイン</a>
      </p>
    </AuthFrame>
  );
}

export function ResetPassword() {
  const [loginId, setLoginId] = useState("");
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [again, setAgain] = useState("");
  const [sentTo, setSentTo] = useState("");
  const [step, setStep] = useState<"mail" | "code" | "password">("mail");
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (step === "code" && code.length === 6) setStep("password");
  }, [code, step]);

  const send = async (event?: React.FormEvent) => {
    event?.preventDefault();
    setError("");
    setNotice("");
    setBusy(true);
    const email = loginId.trim().toLowerCase();
    try {
      await requestPasswordReset(email);
      setSentTo(email);
      setCode("");
      setStep("code");
      setNotice("登録されているメールアドレスなら、再設定用の確認コードを送りました。");
    } catch (err) {
      setError(messageFor(err));
    } finally {
      setBusy(false);
    }
  };

  const finish = async (event: React.FormEvent) => {
    event.preventDefault();
    setError("");
    if (password !== again) {
      setError("パスワードが一致しません。");
      return;
    }
    setBusy(true);
    try {
      await resetPassword(sentTo, code, password);
      location.hash = "#/";
    } catch (err) {
      const codeName = err instanceof ApiError ? err.code : "";
      if (codeName === "invalid_code" || codeName === "code_expired" || codeName === "code_locked") {
        setCode("");
        setStep("code");
      }
      setError(messageFor(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthFrame title="パスワードの再設定">
      <p className="muted">
        {step === "mail" && "登録したメールアドレスを入力してください。"}
        {step === "code" && "届いた6桁の数字を入力してください。"}
        {step === "password" && "新しいパスワードを決めてください。"}
      </p>
      <form
        className="form"
        onSubmit={(event) => {
          event.preventDefault();
          if (step === "mail") void send(event);
          if (step === "password") void finish(event);
        }}
      >
        {step === "mail" ? (
          <div className="field">
            <label htmlFor="reset-id">ログインID（メールアドレス）</label>
            <input id="reset-id" type="email" autoComplete="email" autoCapitalize="none" spellCheck={false} value={loginId} onChange={(event) => setLoginId(event.target.value)} required autoFocus aria-describedby="reset-id-hint" />
            <p className="hint" id="reset-id-hint">{LOGIN_ID_RULE}</p>
          </div>
        ) : null}
        {step === "code" ? (
          <DigitCode id="reset-code" value={code} onChange={setCode} hint={`${sentTo} に届いた6桁の数字です。10分間有効です。6桁そろうと次へ進みます。`} />
        ) : null}
        {step === "password" ? (
          <>
            <div className="field">
              <label htmlFor="reset-password">新しいパスワード</label>
              <input id="reset-password" type="password" autoComplete="new-password" minLength={8} maxLength={128} value={password} onChange={(event) => setPassword(event.target.value)} required autoFocus aria-describedby="reset-password-hint" />
              <p className="hint" id="reset-password-hint">{PASSWORD_RULE}</p>
            </div>
            <div className="field">
              <label htmlFor="reset-again">新しいパスワード（確認）</label>
              <input id="reset-again" type="password" autoComplete="new-password" value={again} onChange={(event) => setAgain(event.target.value)} required />
            </div>
          </>
        ) : null}
        {notice ? <div className="note">{notice}</div> : null}
        <Alert error={error} />
        <div className="actions">
          {step === "mail" ? (
            <button className="btn primary big" type="submit" disabled={busy}>{busy ? "送っています…" : "再設定メールを送る"}</button>
          ) : null}
          {step === "code" ? (
            <>
              <button className="btn" type="button" disabled={busy} onClick={() => void send()}>確認コードを再送</button>
              <button className="btn" type="button" disabled={busy} onClick={() => { setStep("mail"); setSentTo(""); setCode(""); setNotice(""); setError(""); }}>メールアドレスを変更</button>
            </>
          ) : null}
          {step === "password" ? (
            <button className="btn primary big" type="submit" disabled={busy}>{busy ? "設定しています…" : "パスワードを再設定"}</button>
          ) : null}
        </div>
      </form>
      <p>
        <a className="text-link" href="#/login">ログインへ戻る</a>
      </p>
    </AuthFrame>
  );
}
