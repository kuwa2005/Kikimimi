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
  /** 録音中の文字起こし。api は APIキー、chrome は Chrome の音声認識、off は録音だけ。 */
  speech: "api" | "chrome" | "off";
  /** API の文字起こしを、回答やナレッジを付けずに文字だけにする。 */
  transcriptOnly: boolean;
};

export function emptyForm(): MeetingForm {
  // ホームでフォルダを開いていたら、そのフォルダを最初から選んでおく。
  return { title: "", participants: "", templateId: "", questions: [""], folderId: homeFolder(), speech: "api", transcriptOnly: false };
}

const HOME_FOLDER = "kikimimi.homeFolder";

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
  return meetingId == null ? "kikimimi.form.new" : `kikimimi.form.${meetingId}`;
}

export function loadForm(key: string): MeetingForm | null {
  const saved = sessionGet<MeetingForm>(key);
  if (!saved || !Array.isArray(saved.questions)) return null;
  return { ...emptyForm(), ...saved, speech: saved.speech || "api", transcriptOnly: !!saved.transcriptOnly };
}

export function saveForm(key: string, form: MeetingForm) {
  sessionSet(key, form);
}

export function clearForm(key: string) {
  sessionRemove(key);
}

const SETUP_RETURN = "kikimimi.setupReturn";

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
