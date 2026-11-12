// 端末ごとの好み（マイク・タブ音声・テーマ）。保存できなくても動作には影響しない。

export const PREF_MIC = "kikimimi.mic";
export const PREF_TAB = "kikimimi.tab";
export const PREF_THEME = "kikimimi.theme";
export const PREF_PANEL = "kikimimi.panel";

export function pref(key: string, fallback = ""): string {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}

export function savePref(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // 保存できなくても録音には影響しない
  }
}

export function sessionGet<T>(key: string): T | null {
  try {
    const raw = sessionStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

export function sessionSet(key: string, value: unknown) {
  try {
    sessionStorage.setItem(key, JSON.stringify(value));
  } catch {
    // 下書きを残せないだけ
  }
}

export function sessionRemove(key: string) {
  try {
    sessionStorage.removeItem(key);
  } catch {
    // 何もしない
  }
}

/** ログアウト時に、この端末に残した下書きや戻り先を消す。 */
export function clearSessionData() {
  try {
    const keys: string[] = [];
    for (let i = 0; i < sessionStorage.length; i++) {
      const key = sessionStorage.key(i);
      if (key && key.startsWith("kikimimi.")) keys.push(key);
    }
    keys.forEach((key) => sessionStorage.removeItem(key));
  } catch {
    // 何もしない
  }
}

export const PREF_NEWEST_FIRST = "kikimimi.newestFirst";
export const PREF_ENHANCE = "kikimimi.enhance";
