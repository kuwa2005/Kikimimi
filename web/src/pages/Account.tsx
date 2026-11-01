import { useEffect, useState } from "react";
import { api, ApiError, errorMessage, formatAmount } from "../api/client";
import type { Account as AccountData, UsageItem } from "../api/types";
import { fmtLocal } from "../lib/format";
import { useStore } from "../lib/store";
import { logout } from "../state/actions";
import { guarded, useBusy } from "../state/busy";
import { notify } from "../state/notices";
import { changePassword } from "../state/session";
import { setTheme, theme, THEME_LABELS, type ThemeChoice } from "../state/theme";
import { Notices } from "../components/Notices";

function UsageRow({ item }: { item: UsageItem }) {
  const unlimited = item.limit === null;
  const ratio = !unlimited && item.used !== null && item.limit ? Math.min(1, item.used / item.limit) : 0;
  const used = item.used === null ? "会議ごと" : formatAmount(item.used, item.unit);
  const limit = unlimited ? "無制限" : formatAmount(item.limit as number, item.unit);
  return (
    <li className="usage">
      <div className="usage-head">
        <strong>{item.label}</strong>
        <span>
          {item.used === null ? `上限 ${limit}` : `${used} / ${limit}`}
          {item.period_end ? <span className="muted small">（{fmtLocal(item.period_end)} まで）</span> : null}
        </span>
      </div>
      {!unlimited && item.used !== null ? (
        <div className={`bar ${ratio >= 0.9 ? "near" : ""}`} role="progressbar" aria-label={item.label} aria-valuemin={0} aria-valuemax={item.limit as number} aria-valuenow={item.used}>
          <span style={{ width: `${ratio * 100}%` }} />
        </div>
      ) : null}
    </li>
  );
}

export function Account() {
  const [account, setAccount] = useState<AccountData | null>(null);
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");
  const [error, setError] = useState("");
  const busyName = useBusy();
  const choice = useStore(theme);

  useEffect(() => {
    api<AccountData>("/api/account")
      .then(setAccount)
      .catch((err) => notify(errorMessage(err), "error"));
  }, []);

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    setError("");
    if (next !== again) {
      setError("新しいパスワードが一致しません。");
      return;
    }
    void guarded("password", async () => {
      try {
        await changePassword(current, next);
      } catch (err) {
        if (err instanceof ApiError && (err.code === "invalid_credentials" || err.code === "weak_password")) {
          setError(err.code === "invalid_credentials" ? "今のパスワードが違います。" : err.message);
          return;
        }
        throw err;
      }
      setCurrent("");
      setNext("");
      setAgain("");
      notify("パスワードを変更しました。ほかの端末のログインは解除されました。", "info");
    });
  };

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>アカウント</h1>
          {account ? (
            <p className="muted">
              {account.user.login_id} ・ {fmtLocal(account.user.created_at, false)} に登録
            </p>
          ) : null}
        </div>
        <button className="btn" type="button" onClick={() => void logout().catch((err) => notify(errorMessage(err), "error"))}>
          ログアウト
        </button>
      </header>
      <Notices />
      <section className="panel settings-section" aria-labelledby="sec-plan">
        <h2 id="sec-plan">プランと利用量</h2>
        {account ? (
          <>
            <p>
              現在のプラン: <span className="chip plan">{account.plan.name}</span>
            </p>
            <p className="muted small">有料プランは現在ありません。</p>
            <p className="small">
              使える機能: リアルタイム文字起こし（速報字幕）{account.plan.features?.realtime_captions ? " ✓ 使えます" : " — このプランでは使えません"}
            </p>
            <ul className="usage-list">
              {account.usage.map((item) => (
                <UsageRow key={item.item} item={item} />
              ))}
            </ul>
          </>
        ) : (
          <p className="muted">読み込み中</p>
        )}
        <div className="note">
          <p>
            このサービスの Free プランと、Gemini / OpenAI の API 料金・利用枠は別です。登録した自分のキーの契約によっては、提供元の料金が発生します。
          </p>
          <p className="small">
            ここに出しているのはこのサービスの上限と利用量です。Gemini / OpenAI 側の残高や枠は、それぞれの管理画面で確認してください。
          </p>
          {account?.note ? <p className="small">{account.note}</p> : null}
        </div>
      </section>
      <section className="panel settings-section" aria-labelledby="sec-password">
        <h2 id="sec-password">パスワードの変更</h2>
        <form onSubmit={submit} className="form narrow-form">
          <div className="field">
            <label htmlFor="pw-current">今のパスワード</label>
            <input id="pw-current" type="password" autoComplete="current-password" value={current} onChange={(event) => setCurrent(event.target.value)} required />
          </div>
          <div className="field">
            <label htmlFor="pw-next">新しいパスワード</label>
            <input id="pw-next" type="password" autoComplete="new-password" minLength={8} maxLength={128} value={next} onChange={(event) => setNext(event.target.value)} required aria-describedby="pw-hint" />
            <p className="hint" id="pw-hint">
              8〜128 文字（文字の種類は問いません。すべて同じ文字だけのものは使えません）。
            </p>
          </div>
          <div className="field">
            <label htmlFor="pw-again">新しいパスワード（確認）</label>
            <input id="pw-again" type="password" autoComplete="new-password" value={again} onChange={(event) => setAgain(event.target.value)} required />
          </div>
          {error ? (
            <div className="banner error" role="alert">
              {error}
            </div>
          ) : null}
          <div className="actions">
            <button className="btn primary" type="submit" disabled={!!busyName}>
              パスワードを変更
            </button>
          </div>
        </form>
      </section>
      <section className="panel settings-section" aria-labelledby="sec-view">
        <h2 id="sec-view">表示</h2>
        <fieldset className="field">
          <legend>テーマ</legend>
          {(Object.keys(THEME_LABELS) as ThemeChoice[]).map((key) => (
            <label key={key} className="check">
              <input type="radio" name="theme" value={key} checked={choice === key} onChange={() => setTheme(key)} />
              {THEME_LABELS[key]}
            </label>
          ))}
          <p className="hint">「OS に合わせる」は端末のライト／ダークの設定に従います。動きを減らす設定にしている端末では、録音中の点滅を止めます。</p>
        </fieldset>
      </section>
    </div>
  );
}
