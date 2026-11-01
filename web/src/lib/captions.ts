// リアルタイム文字起こし（Chrome の速報字幕, #104）。
// 正の書き起こしはこれまでどおり区間ごとの AI の文字起こし。ここで作る文字は画面に出すだけで、保存しない。
// 既定は端末内の認識（processLocally）だけを使い、音声を端末の外に出さない。
import { createStore } from "./store";
import { captionTrack, recorderState, sentSamples } from "../recorder/recorder";

export type CaptionMode = "off" | "local" | "cloud";

export type CaptionLine = { chunkStart: number; text: string };

export const captions = createStore<{ running: boolean; interim: string; lines: CaptionLine[]; problem: string }>({
  running: false,
  interim: "",
  lines: [],
  problem: "",
});

/* Web Speech API は TypeScript の型に無いことがあるので、使う分だけ定義する。 */
type Recognition = {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  processLocally?: boolean;
  phrases?: unknown[];
  onresult: ((event: { resultIndex: number; results: ArrayLike<{ isFinal: boolean; 0: { transcript: string } }> }) => void) | null;
  onerror: ((event: { error: string }) => void) | null;
  onend: (() => void) | null;
  start: (track?: MediaStreamTrack) => void;
  stop: () => void;
  abort: () => void;
};
type RecognitionClass = {
  new (): Recognition;
  available?: (options: { langs: string[]; processLocally: boolean }) => Promise<string>;
  install?: (options: { langs: string[]; processLocally: boolean }) => Promise<boolean>;
};

function recognitionClass(): RecognitionClass | null {
  const w = window as unknown as { SpeechRecognition?: RecognitionClass; webkitSpeechRecognition?: RecognitionClass };
  return w.SpeechRecognition || w.webkitSpeechRecognition || null;
}

const LANG = "ja-JP";

/** この端末で使えるか。"available" / "downloadable" / "downloading" / "unavailable" / "unsupported"。 */
export async function captionAvailability(mode: CaptionMode): Promise<string> {
  const SR = recognitionClass();
  if (!SR) return "unsupported";
  if (!SR.available) return mode === "cloud" ? "available" : "unsupported";
  try {
    return await SR.available({ langs: [LANG], processLocally: mode !== "cloud" });
  } catch {
    return "unsupported";
  }
}

/** 端末内の認識に使う日本語の言語パックを入れる（Chrome がダウンロードする）。 */
export async function installLocalCaptions(): Promise<boolean> {
  const SR = recognitionClass();
  if (!SR?.install) return false;
  try {
    return await SR.install({ langs: [LANG], processLocally: true });
  } catch {
    return false;
  }
}

let mode: CaptionMode = "off";
let phrases: string[] = [];
let recognition: Recognition | null = null;
let wanted = false;
let restarts = 0;

/** 会議の画面から設定を渡す。mode が off なら止める。 */
export function configureCaptions(next: CaptionMode) {
  mode = next;
  sync();
}

/** 当てやすくする語（議題・参加者名）。動いている認識は止めず、次に始め直すときから使う。 */
export function setCaptionPhrases(words: string[]) {
  phrases = words.filter(Boolean).slice(0, 50);
}

/** AI の書き起こしが届いた区間までの速報は消す（置き換わったため）。 */
export function dropCaptionsUpTo(chunkStart: number) {
  const state = captions.get();
  const lines = state.lines.filter((line) => line.chunkStart > chunkStart + 0.05);
  if (lines.length !== state.lines.length) captions.set({ ...state, lines });
}

function setState(patch: Partial<ReturnType<typeof captions.get>>) {
  captions.set({ ...captions.get(), ...patch });
}

function stop() {
  wanted = false;
  const current = recognition;
  recognition = null;
  if (current) {
    current.onend = null;
    try {
      current.abort();
    } catch {
      // すでに止まっている
    }
  }
  setState({ running: false, interim: "", lines: [] });
}

function start() {
  const SR = recognitionClass();
  const track = captionTrack();
  if (!SR || !track) return;
  wanted = true;
  const rec = new SR();
  rec.lang = LANG;
  rec.continuous = true;
  rec.interimResults = true;
  if ("processLocally" in rec) rec.processLocally = mode !== "cloud";
  // 固有名詞の当たりやすさ（対応している Chrome だけ）。
  const Phrase = (window as unknown as { SpeechRecognitionPhrase?: new (text: string, boost: number) => unknown }).SpeechRecognitionPhrase;
  if (Phrase && "phrases" in rec && phrases.length) {
    try {
      rec.phrases = phrases.map((word) => new Phrase(word.slice(0, 40), 3));
    } catch {
      // 使えなければ付けない
    }
  }
  rec.onresult = (event) => {
    let interim = "";
    const finals: CaptionLine[] = [];
    for (let i = event.resultIndex; i < event.results.length; i++) {
      const result = event.results[i];
      const text = result[0].transcript.trim();
      if (!text) continue;
      if (result.isFinal) finals.push({ chunkStart: sentSamples() / 16000, text });
      else interim += text;
    }
    const state = captions.get();
    setState({ interim, lines: finals.length ? [...state.lines, ...finals].slice(-40) : state.lines, problem: "" });
  };
  rec.onerror = (event) => {
    const reasons: Record<string, string> = {
      "language-not-supported": "この端末では日本語の端末内認識が使えません（設定で言語パックを入れてください）",
      "not-allowed": "音声認識が許可されていません",
      "service-not-allowed": "音声認識が許可されていません",
      network: "音声認識のサービスに接続できません",
    };
    if (event.error === "no-speech" || event.error === "aborted") return;
    setState({ problem: reasons[event.error] || `速報字幕を止めました（${event.error}）` });
    if (reasons[event.error]) wanted = false;
  };
  rec.onend = () => {
    // 連続認識は途中で終わることがあるので、録音中なら始め直す（回数に上限）。
    if (wanted && recognition === rec && recorderState.get().capturing && restarts < 500) {
      restarts += 1;
      setTimeout(() => {
        if (wanted && recognition === rec) {
          try {
            rec.start(captionTrack() || undefined);
          } catch {
            // 始め直せなければ次の録音開始まで待つ
          }
        }
      }, 300);
    } else if (recognition === rec) {
      setState({ running: false, interim: "" });
    }
  };
  try {
    rec.start(track);
    recognition = rec;
    restarts = 0;
    setState({ running: true, problem: "" });
  } catch (err) {
    setState({ running: false, problem: `速報字幕を始められません: ${(err as Error).message}` });
  }
}

/** 録音中で、設定が ON のときだけ動かす。 */
function sync() {
  const capturing = recorderState.get().capturing;
  const shouldRun = mode !== "off" && capturing && !!captionTrack();
  if (shouldRun && !recognition) start();
  else if (!shouldRun && recognition) stop();
}

recorderState.subscribe(() => sync());
