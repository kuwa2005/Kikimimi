import { createStore } from "../lib/store";
import { pref, PREF_THEME, savePref } from "../lib/prefs";

// OS に合わせる／ライト／ダーク。選択は端末に残し、初期表示は index.html で先に当てる。
export type ThemeChoice = "system" | "light" | "dark";

const media = window.matchMedia("(prefers-color-scheme: dark)");

function initial(): ThemeChoice {
  const saved = pref(PREF_THEME, "system");
  return saved === "light" || saved === "dark" ? saved : "system";
}

export const theme = createStore<ThemeChoice>(initial());

function apply() {
  const choice = theme.get();
  const dark = choice === "dark" || (choice === "system" && media.matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}

export function setTheme(choice: ThemeChoice) {
  theme.set(choice);
  savePref(PREF_THEME, choice);
  apply();
}

media.addEventListener("change", apply);
apply();

export const THEME_LABELS: Record<ThemeChoice, string> = {
  system: "OS に合わせる",
  light: "ライト",
  dark: "ダーク",
};
