// リアルタイム文字起こし（Chrome の速報字幕, #104）。
// 会議の文字起こしが API のときは、ここは画面の速報だけで保存しない。
// 会議が Chrome の音声認識のときは、確定した文を保存する（setCaptionPersist）。
// 既定は端末内の認識（processLocally）だけを使い、音声を端末の外に出さない。
import { createStore } from "./store";
import { captionTrack, localSeconds, recorderState } from "../recorder/recorder";

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

let persist: ((line: CaptionLine) => void) | null = null;
let lastAt = -1;

/** Chrome の確定文をサーバへ残す。null なら画面に出すだけ。 */
export function setCaptionPersist(next: ((line: CaptionLine) => void) | null) {
  persist = next;
  lastAt = -1;
}

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
let fallbackNote = "";
let preferTrack = false;

/** 会議の文字起こしに使う Chrome の認識。端末内が使えるときはそれ、無いときは Chrome 標準の音声認識。 */
export async function chromeCaptionMode(): Promise<CaptionMode> {
  if (!recognitionClass()) return "off";
  const local = await captionAvailability("local");
  return local === "available" ? "local" : "cloud";
}

/** 会議の画面から設定を渡す。mode が off なら止める。 */
export function configureCaptions(next: CaptionMode) {
  mode = next;
  fallbackNote = "";
  preferTrack = false;
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

function listen(rec: Recognition) {
  const track = captionTrack();
  // 端末内認識と、マイクをすでに掴んでいるときは、録音中の音声を渡す。
  // それ以外の Chrome 標準の認識は、ブラウザがマイクを取る。
  if (mode === "cloud" && !preferTrack) {
    rec.start();
    return;
  }
  if (!track) throw new Error("マイクの音がありません");
  rec.start(track);
}

function start() {
  const SR = recognitionClass();
  if (!SR) return;
  if (mode !== "cloud" && !captionTrack()) return;
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
      if (result.isFinal) {
        let at = localSeconds();
        if (at <= lastAt + 0.2) at = lastAt + 0.25;
        lastAt = at;
        finals.push({ chunkStart: at, text });
      } else interim += text;
    }
    const state = captions.get();
    setState({ interim, lines: finals.length ? [...state.lines, ...finals].slice(-40) : state.lines, problem: fallbackNote });
    if (persist) finals.forEach((line) => persist?.(line));
  };
  rec.onerror = (event) => {
    if (event.error === "no-speech" || event.error === "aborted") return;
    // 端末内の言語パックが無いときは、Chrome 標準の音声認識で続ける。
    if (event.error === "language-not-supported" && mode === "local") {
      mode = "cloud";
      fallbackNote = "端末内の日本語認識が使えないため、Chrome の音声認識を使っています。";
      wanted = true;
      recognition = null;
      rec.onend = null;
      try {
        rec.abort();
      } catch {
        // すでに止まっている
      }
      setState({ running: false, problem: fallbackNote });
      setTimeout(() => sync(), 300);
      return;
    }
    if (event.error === "audio-capture" && !preferTrack && captionTrack()) {
      preferTrack = true;
      wanted = true;
      recognition = null;
      rec.onend = null;
      try {
        rec.abort();
      } catch {
        // すでに止まっている
      }
      setTimeout(() => sync(), 300);
      return;
    }
    const reasons: Record<string, string> = {
      "language-not-supported": "この端末では日本語の音声認識が使えません",
      "not-allowed": "音声認識が許可されていません",
      "service-not-allowed": "音声認識が許可されていません",
      "audio-capture": "マイクを音声認識に渡せません",
      network: "音声認識のサービスに接続できません",
    };
    setState({ problem: reasons[event.error] || `音声認識を止めました（${event.error}）` });
    if (reasons[event.error]) wanted = false;
  };
  rec.onend = () => {
    // 連続認識は途中で終わることがあるので、録音中なら始め直す（回数に上限）。
    if (wanted && recognition === rec && recorderState.get().capturing && restarts < 500) {
      restarts += 1;
      setTimeout(() => {
        if (wanted && recognition === rec) {
          try {
            listen(rec);
          } catch {
            // 始め直せなければ次の録音開始まで待つ
          }
        }
      }, 300);
    } else if (recognition === rec) {
      setState({ running: false, interim: "", problem: fallbackNote });
    }
  };
  try {
    listen(rec);
    recognition = rec;
    restarts = 0;
    setState({ running: true, problem: fallbackNote });
  } catch (err) {
    if (mode === "local") {
      mode = "cloud";
      fallbackNote = "端末内の日本語認識が使えないため、Chrome の音声認識を使っています。";
      try {
        rec.processLocally = false;
        rec.start();
        recognition = rec;
        restarts = 0;
        setState({ running: true, problem: fallbackNote });
        return;
      } catch {
        // 下のメッセージへ
      }
    }
    setState({ running: false, problem: `音声認識を始められません: ${(err as Error).message}` });
  }
}

/** 録音中で、Chrome の音声認識を動かす。 */
function sync() {
  const capturing = recorderState.get().capturing;
  const hasEngine = !!recognitionClass();
  const canHear = mode === "cloud" || !!captionTrack();
  const shouldRun = mode !== "off" && capturing && hasEngine && canHear;
  if (mode !== "off" && capturing && !hasEngine) {
    setState({ running: false, problem: "このブラウザは Chrome の音声認識に対応していません。デスクトップの Chrome で開いてください。" });
  } else if (mode === "local" && capturing && !captionTrack() && !recognition) {
    setState({ problem: "音声認識に渡すマイクの音がまだありません。" });
  }
  if (shouldRun && !recognition) start();
  else if (!shouldRun && recognition) stop();
}

recorderState.subscribe(() => sync());
