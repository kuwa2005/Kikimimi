import { sessionGet, sessionRemove, sessionSet } from "../lib/prefs";

// 会議の準備中の入力。設定画面との往復やテンプレート保存で消えないよう、タブの間だけ残す。
export type MeetingForm = {
  title: string;
  participants: string;
  templateId: string;
  questions: string[];
  /** 議題ごとの目安時間（分、文字のまま）。questions と同じ並び。 */
  minutes?: string[];
  /** 新しい会議を入れるフォルダ（"" はフォルダなし）。作成時だけ使う。 */
  folderId?: string;
};

export function emptyForm(): MeetingForm {
  // ホームでフォルダを開いていたら、そのフォルダを最初から選んでおく。
  return { title: "", participants: "", templateId: "", questions: [""], folderId: homeFolder() };
}

const HOME_FOLDER = "gijiroku.homeFolder";

/** ホームで開いているフォルダ（"" はすべて、"none" はフォルダなし、数字はフォルダ）。 */
export function homeFolder(): string {
  const value = sessionGet<string>(HOME_FOLDER) || "";
  return /^\d+$/.test(value) ? value : "";
}

export function homeFolderFilter(): string {
  return sessionGet<string>(HOME_FOLDER) || "";
}

export function setHomeFolder(value: string) {
  sessionSet(HOME_FOLDER, value);
}

export function formKey(meetingId: number | null): string {
  return meetingId == null ? "gijiroku.form.new" : `gijiroku.form.${meetingId}`;
}

export function loadForm(key: string): MeetingForm | null {
  const saved = sessionGet<MeetingForm>(key);
  if (!saved || !Array.isArray(saved.questions)) return null;
  return saved;
}

export function saveForm(key: string, form: MeetingForm) {
  sessionSet(key, form);
}

export function clearForm(key: string) {
  sessionRemove(key);
}

const SETUP_RETURN = "gijiroku.setupReturn";

/** 初回設定へ案内したとき、設定が済んだら戻る先。 */
export function setSetupReturn(hash: string) {
  sessionSet(SETUP_RETURN, hash);
}

export function setupReturn(): string | null {
  return sessionGet<string>(SETUP_RETURN);
}

export function clearSetupReturn() {
  sessionRemove(SETUP_RETURN);
}
