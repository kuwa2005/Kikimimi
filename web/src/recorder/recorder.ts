// 録音ループはネットワークを待たない。
// 音声スレッドが溜めたサンプルを、無音の切れ目（長くても 15 秒）で区切って送信待ちへ載せるだけ。
// 文字起こし・回答・ナレッジ検索はサーバの後段で進み、マイクは止めない。
// React の外に置き、画面部品のマウント・アンマウント（StrictMode の二重実行を含む）に左右されない。

import { ApiError, limitText, request } from "../api/client";
import type { UsageItem } from "../api/types";
import { createStore } from "../lib/store";
import { pref, PREF_MIC, PREF_TAB, savePref } from "../lib/prefs";
import { notify, clearNotice } from "../state/notices";
import * as vault from "./vault";

const CHUNK = {
  min: 4, // 秒。これより短い区切りは作らない
  max: 15, // 秒。話し続けていてもここで区切る
  silence: 0.6, // 秒。この長さの無音で区切る
  frame: 0.02, // 秒。無音判定の単位
  minSpeech: 0.3, // 秒。区間内の発話がこれ未満なら「無音」として文字起こしに回さない
};

type QueueItem = {
  meetingId: number;
  userId: number;
  startMs: number;
  wav: Uint8Array;
  silent: boolean;
  /** 端末の保管庫に書き終えたときの番号（書けなかったら null）。 */
  saved: Promise<number | null>;
  /** 会議が録音を受け付けなくなったあとに残った区間。取り込みとして送る。 */
  late?: boolean;
};

type CaptionItem = { meetingId: number; userId: number; startSec: number; text: string; saved: Promise<number | null> };

/** 途中保存（tail）の間隔。ブラウザが落ちても失うのは最大でこの長さ。 */
const TAIL_EVERY_MS = 2000;
/** 録音を受け付けなくなった会議の区間を、取り込みとして送り直す間隔。 */
const PARKED_RETRY_MS = 30000;

const cap = {
  capturing: false,
  meetingId: null as number | null,
  userId: null as number | null,
  stream: null as MediaStream | null,
  tabStream: null as MediaStream | null,
  /** マイクとタブ音声を混ぜた音声（速報字幕に渡す, #104）。 */
  mixTrack: null as MediaStreamTrack | null,
  ctx: null as AudioContext | null,
  sampleRate: 16000,
  fir: null as { rate: number; taps: Float32Array } | null,
  sentSamples: 0,
  buffers: [] as Float32Array[],
  buffered: 0,
  queue: [] as QueueItem[],
  /** 送り直しても今は受け付けられない区間（会議の終了後・上限など）。端末に残し、取り込みとして送る。 */
  parked: [] as QueueItem[],
  parkedPumping: false,
  captionQueue: [] as CaptionItem[],
  captionPumping: false,
  pumping: false,
  paused: false, // ログインが切れた間は送らない
  level: 0,
  ready: null as Promise<void> | null,
  lastPcmAt: 0,
  micProblem: "",
  recovering: false,
  recoveries: 0,
  lastRecoveryAt: 0,
  deviceLabel: "",
  /** 端末の保管庫に書けているか。書けないときは画面に知らせる。 */
  vaultOk: true,
  tailAt: 0,
  tailWriting: false,
  tailMeeting: null as number | null,
  restoredFor: null as number | null,
  wakeLock: null as { release: () => Promise<void>; released?: boolean } | null,
  releaseLock: null as (() => void) | null,
  includeTab: false,
  vad: { floor: 0.005, energy: 0, count: 0, speech: 0, silenceRun: 0 },
};

export type RecorderSnapshot = {
  capturing: boolean;
  meetingId: number | null;
  streamOpen: boolean;
  tabShared: boolean;
  deviceLabel: string;
  micProblem: string;
  queued: Record<number, number>;
  queueLength: number;
  parked: number;
  captionsPending: number;
  vaultOk: boolean;
  pumping: boolean;
  paused: boolean;
};

function snapshot(): RecorderSnapshot {
  const queued: Record<number, number> = {};
  for (const item of cap.queue) queued[item.meetingId] = (queued[item.meetingId] || 0) + 1;
  return {
    capturing: cap.capturing,
    meetingId: cap.meetingId,
    streamOpen: !!cap.stream,
    tabShared: !!cap.tabStream,
    deviceLabel: cap.deviceLabel,
    micProblem: cap.micProblem,
    queued,
    queueLength: cap.queue.length,
    parked: cap.parked.length,
    captionsPending: cap.captionQueue.length,
    vaultOk: cap.vaultOk,
    pumping: cap.pumping,
    paused: cap.paused,
  };
}

export const recorderState = createStore<RecorderSnapshot>(snapshot());
export const devices = createStore<MediaDeviceInfo[]>([]);

function publish() {
  const next = snapshot();
  const prev = recorderState.get();
  const same =
    prev.capturing === next.capturing &&
    prev.meetingId === next.meetingId &&
    prev.streamOpen === next.streamOpen &&
    prev.tabShared === next.tabShared &&
    prev.deviceLabel === next.deviceLabel &&
    prev.micProblem === next.micProblem &&
    prev.queueLength === next.queueLength &&
    prev.parked === next.parked &&
    prev.captionsPending === next.captionsPending &&
    prev.vaultOk === next.vaultOk &&
    prev.pumping === next.pumping &&
    prev.paused === next.paused &&
    JSON.stringify(prev.queued) === JSON.stringify(next.queued);
  if (!same) recorderState.set(next);
}

/** 上限に当たった（録音を止めた）ことを画面に知らせる。終了を促すダイアログは画面側で出す。 */
type LimitHandler = (meetingId: number, limit: UsageItem) => void;
let limitHandler: LimitHandler = () => {};
export function onLimitReached(handler: LimitHandler) {
  limitHandler = handler;
}

/** 別の画面で会議が終わったなど、録音を止めたときの知らせ。 */
export function stopCapturing(message: string) {
  if (!cap.capturing) return;
  // 溜まっている音声も捨てずに区間にする（送れなければ端末に残る）。
  cutChunk();
  cap.capturing = false;
  resetVad();
  releaseGuards();
  stopStream();
  notify(message, "warn", "mic");
  publish();
}

export function recordingHere(id: number | null | undefined): boolean {
  return cap.capturing && id != null && cap.meetingId === id;
}

export function level(): number {
  return cap.stream ? cap.level : 0;
}

export function localSeconds(): number {
  const pending = cap.buffered * (16000 / (cap.sampleRate || 16000));
  return (cap.sentSamples + pending) / 16000;
}

/** 速報字幕に渡す音声（マイクとタブ音声を混ぜたもの）。マイクを開いていなければ null。 */
export function captionTrack(): MediaStreamTrack | null {
  return cap.mixTrack && cap.mixTrack.readyState === "live" ? cap.mixTrack : null;
}

export function sentSamples(): number {
  return cap.sentSamples;
}

function noteMicProblem(message: string) {
  cap.micProblem = message;
  publish();
}

function watchMic(stream: MediaStream, ctx: AudioContext) {
  const track = stream.getAudioTracks()[0];
  if (track) {
    const lost = () => {
      if (!cap.capturing) return;
      noteMicProblem("マイクから音が届いていません。接続を確認してください。録音の受付は続けています。");
      void recoverMic();
    };
    track.addEventListener("ended", lost);
    track.addEventListener("mute", lost);
  }
  ctx.onstatechange = () => {
    if (!cap.capturing) return;
    const state = ctx.state as string;
    if (state === "suspended" || state === "interrupted") {
      noteMicProblem("マイクの処理が止まっています。復帰を試みます。");
      ctx.resume().catch(() => {});
    }
  };
}

async function recoverMic() {
  // 1 回で諦めず、10 秒おきに取り直す（USB マイクの抜き差し、スリープ復帰など）。
  if (cap.recovering || Date.now() - cap.lastRecoveryAt < 10000) return;
  cap.recovering = true;
  cap.recoveries += 1;
  cap.lastRecoveryAt = Date.now();
  try {
    stopStream();
    await ensureAudio();
    if (!cap.capturing) return;
    cap.micProblem = "";
    cap.lastPcmAt = Date.now();
    notify("マイクを取り直しました。", "info", "mic");
    publish();
  } catch (err) {
    noteMicProblem("マイクを取り直せません。" + (err instanceof Error ? err.message : String(err)));
  } finally {
    cap.recovering = false;
  }
}

function takeSamples(count: number): Float32Array {
  const out = new Float32Array(count);
  let filled = 0;
  while (filled < count && cap.buffers.length) {
    const buf = cap.buffers[0];
    const need = count - filled;
    if (buf.length <= need) {
      out.set(buf, filled);
      filled += buf.length;
      cap.buffers.shift();
      cap.buffered -= buf.length;
    } else {
      out.set(buf.subarray(0, need), filled);
      cap.buffers[0] = buf.subarray(need);
      cap.buffered -= need;
      filled += need;
    }
  }
  return out;
}

// 16kHz で録れなかったときだけ使う。間引く前に 7kHz より上を落とし、折り返し雑音を防ぐ。
function lowpass(rate: number): Float32Array {
  if (cap.fir && cap.fir.rate === rate) return cap.fir.taps;
  const size = 31;
  const cutoff = 7000 / rate;
  const taps = new Float32Array(size);
  let sum = 0;
  for (let i = 0; i < size; i++) {
    const n = i - (size - 1) / 2;
    const sinc = n === 0 ? 2 * cutoff : Math.sin(2 * Math.PI * cutoff * n) / (Math.PI * n);
    const window = 0.54 - 0.46 * Math.cos((2 * Math.PI * i) / (size - 1));
    taps[i] = sinc * window;
    sum += taps[i];
  }
  for (let i = 0; i < size; i++) taps[i] /= sum;
  cap.fir = { rate, taps };
  return taps;
}

function resample(input: Float32Array, inRate: number): Float32Array {
  if (inRate === 16000) return input;
  const taps = lowpass(inRate);
  const half = (taps.length - 1) / 2;
  const ratio = inRate / 16000;
  const length = Math.floor(input.length / ratio);
  const out = new Float32Array(length);
  for (let i = 0; i < length; i++) {
    const center = Math.round(i * ratio);
    let acc = 0;
    for (let k = 0; k < taps.length; k++) {
      const index = center + k - half;
      if (index >= 0 && index < input.length) acc += input[index] * taps[k];
    }
    out[i] = acc;
  }
  return out;
}

export function encodeWav(samples: Float32Array, rate: number): Uint8Array {
  const buffer = new ArrayBuffer(44 + samples.length * 2);
  const view = new DataView(buffer);
  const write = (offset: number, text: string) => {
    for (let i = 0; i < text.length; i++) view.setUint8(offset + i, text.charCodeAt(i));
  };
  write(0, "RIFF");
  view.setUint32(4, 36 + samples.length * 2, true);
  write(8, "WAVE");
  write(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, rate, true);
  view.setUint32(28, rate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  write(36, "data");
  view.setUint32(40, samples.length * 2, true);
  let offset = 44;
  for (let i = 0; i < samples.length; i++) {
    let sample = samples[i];
    if (sample > 1) sample = 1;
    if (sample < -1) sample = -1;
    view.setInt16(offset, sample < 0 ? sample * 0x8000 : sample * 0x7fff, true);
    offset += 2;
  }
  return new Uint8Array(buffer);
}

function resetVad() {
  Object.assign(cap.vad, { energy: 0, count: 0, speech: 0, silenceRun: 0 });
}

// 20ms ごとの音量で発話か無音かを判定する。雑音の大きさには追従させる。
function closeFrame(power: number) {
  const vad = cap.vad;
  const rms = Math.sqrt(power);
  // 雑音の目安は静かな方へはすぐ下げ、上げるのはゆっくり（話し続けても発話を無音と取り違えない）。
  vad.floor = rms < vad.floor ? vad.floor * 0.8 + rms * 0.2 : Math.min(vad.floor * 1.001, rms);
  const speech = rms > Math.max(vad.floor * 3, 0.006);
  if (speech) {
    vad.speech += 1;
    vad.silenceRun = 0;
  } else {
    vad.silenceRun += 1;
  }
  const seconds = cap.buffered / cap.sampleRate;
  const quietFor = vad.silenceRun * CHUNK.frame;
  const spoke = vad.speech * CHUNK.frame >= CHUNK.minSpeech;
  // 発話のあとの切れ目で区切る。黙ったままの間は最長まで 1 区間にまとめる。
  if ((spoke && seconds >= CHUNK.min && quietFor >= CHUNK.silence) || seconds >= CHUNK.max) cutChunk();
}

function onPcm(samples: Float32Array) {
  if (!samples || !samples.length) return;
  cap.lastPcmAt = Date.now();
  if (cap.micProblem) {
    cap.micProblem = "";
    cap.recoveries = 0;
    publish();
  }
  let energy = 0;
  for (let i = 0; i < samples.length; i += 16) energy += samples[i] * samples[i];
  cap.level = Math.sqrt(energy / Math.max(1, samples.length / 16));
  if (!cap.capturing) return;
  cap.buffers.push(samples);
  cap.buffered += samples.length;
  const frameLen = Math.max(1, Math.round(cap.sampleRate * CHUNK.frame));
  const vad = cap.vad;
  for (let i = 0; i < samples.length; i++) {
    vad.energy += samples[i] * samples[i];
    vad.count += 1;
    if (vad.count >= frameLen) {
      const power = vad.energy / vad.count;
      vad.energy = 0;
      vad.count = 0;
      closeFrame(power);
    }
  }
  if (Date.now() - cap.tailAt >= TAIL_EVERY_MS) void writeTail();
}

/** 区切る前の録音を、端末の保管庫に途中保存する。ブラウザが落ちても次回に区間として送れる。 */
async function writeTail() {
  if (cap.tailWriting || cap.meetingId == null || cap.userId == null) return;
  cap.tailAt = Date.now();
  if (!cap.capturing || !cap.buffered) return;
  cap.tailWriting = true;
  try {
    const joined = new Float32Array(cap.buffered);
    let offset = 0;
    for (const buf of cap.buffers) {
      joined.set(buf, offset);
      offset += buf.length;
    }
    const pcm = vault.pcmToInt16(resample(joined, cap.sampleRate));
    const startMs = Math.round(cap.sentSamples / 16);
    cap.tailMeeting = cap.meetingId;
    await vault.putTail({ userId: cap.userId, meetingId: cap.meetingId, startMs, pcm: pcm.buffer as ArrayBuffer, updatedAt: Date.now() });
    vaultWorks();
  } catch (err) {
    vaultFailed(err);
  } finally {
    cap.tailWriting = false;
  }
}

function vaultWorks() {
  if (!cap.vaultOk) {
    cap.vaultOk = true;
    clearNotice("vault");
    publish();
  }
}

function vaultFailed(err: unknown) {
  if (cap.vaultOk) {
    cap.vaultOk = false;
    notify(
      "この端末に録音の控えを保存できません（" +
        (err instanceof Error ? err.message : String(err)) +
        "）。録音と送信は続けていますが、ブラウザが落ちると未送信分を失います。シークレットウィンドウではなく通常の Chrome で開いてください。",
      "error",
      "vault",
    );
    publish();
  }
}

function persistChunk(item: Omit<QueueItem, "saved">): Promise<number | null> {
  return vault
    .putChunk({
      userId: item.userId,
      meetingId: item.meetingId,
      startMs: item.startMs,
      wav: item.wav.buffer.slice(item.wav.byteOffset, item.wav.byteOffset + item.wav.byteLength) as ArrayBuffer,
      silent: item.silent,
      createdAt: Date.now(),
      late: item.late,
    })
    .then((id) => {
      vaultWorks();
      return id;
    })
    .catch((err) => {
      vaultFailed(err);
      return null;
    });
}

/** サーバが受け取った（または二度と要らない）区間を、保管庫から消す。 */
function forget(item: QueueItem) {
  void item.saved.then((id) => (id == null ? undefined : vault.deleteChunk(id).catch(() => {})));
}

// 溜まった音声を 1 区間として送信待ちへ載せる。区間には送り先の会議と利用者を持たせる。
export function cutChunk() {
  if (!cap.buffered || cap.meetingId == null || cap.userId == null) return;
  const piece = takeSamples(cap.buffered);
  const down = resample(piece, cap.sampleRate);
  const silent = cap.vad.speech * CHUNK.frame < CHUNK.minSpeech;
  resetVad();
  if (!down.length) return;
  const startMs = Math.round(cap.sentSamples / 16);
  cap.sentSamples += down.length;
  const base = { meetingId: cap.meetingId, userId: cap.userId, startMs, wav: encodeWav(down, 16000), silent };
  // 送る前に端末へ書く。送信が成功したら消す。
  cap.queue.push({ ...base, saved: persistChunk(base) });
  publish();
  void pumpUploads();
}

function audioHeaders(item: QueueItem): Record<string, string> {
  const headers: Record<string, string> = {
    "Content-Type": "audio/wav",
    "X-Start-Ms": String(item.startMs),
    "X-Silent": item.silent ? "1" : "0",
  };
  if (item.late) headers["X-Import"] = "1";
  return headers;
}

/** 今は受け付けられない区間を、捨てずに脇へ置く（端末には残す）。 */
function park(items: QueueItem[], message: string) {
  if (!items.length) return;
  for (const item of items) {
    if (!item.late) {
      item.late = true;
      void item.saved.then((id) => (id == null ? undefined : vault.markLate(id).catch(() => {})));
    }
    cap.parked.push(item);
  }
  notify(message, "warn", "upload-drop");
  publish();
}

async function pumpUploads() {
  if (cap.pumping || cap.paused) return;
  cap.pumping = true;
  publish();
  let delay = 800;
  try {
    while (cap.queue.length && !cap.paused) {
      const item = cap.queue[0];
      try {
        await request(`/api/meetings/${item.meetingId}/audio`, {
          method: "POST",
          headers: audioHeaders(item),
          raw: item.wav as unknown as BodyInit,
        });
        cap.queue.shift();
        forget(item);
        delay = 800;
        clearNotice("upload");
        publish();
      } catch (err) {
        const status = err instanceof ApiError ? err.status : 0;
        const message = err instanceof Error ? err.message : String(err);
        if (status === 401) {
          // ログインが切れた。同じ利用者でログインし直すまで送らない（別の利用者へは送らない）。
          cap.paused = true;
          notify(
            `ログインが切れたため、録音の送信を止めています（未送信 ${cap.queue.length} 区間はこの端末に保存済み）。同じアカウントでログインし直すと続きを送ります。`,
            "error",
            "upload",
          );
          break;
        }
        if (status === 404) {
          // 会議が削除された。送り先が無い。
          cap.queue.shift();
          forget(item);
          if (recordingHere(item.meetingId)) stopCapturing("この会議は削除されたため、録音を止めました。");
          publish();
          continue;
        }
        if (status === 403 && err instanceof ApiError && err.code === "limit_exceeded" && err.limit) {
          // サービスの上限。この会議の録音を止める。区間は捨てずに端末へ残す。
          const held = cap.queue.filter((q) => q.meetingId === item.meetingId);
          cap.queue = cap.queue.filter((q) => q.meetingId !== item.meetingId);
          if (recordingHere(item.meetingId)) stopCapturing("上限に達したため、録音を止めました。");
          const rest = cap.queue.filter((q) => q.meetingId === item.meetingId);
          cap.queue = cap.queue.filter((q) => q.meetingId !== item.meetingId);
          park([...held, ...rest], `${limitText(err.limit)}未送信の ${held.length + rest.length} 区間はこの端末に保存しています。`);
          limitHandler(item.meetingId, err.limit);
          continue;
        }
        if (status >= 400 && status < 500 && status !== 408 && status !== 429) {
          // 会議が終了したなど、今は受け付けられない。捨てずに端末へ残し、あとで取り込みとして送る。
          cap.queue.shift();
          if (status === 409 && recordingHere(item.meetingId) && !item.late) {
            stopCapturing("この会議は録音を受け付けなくなったため、録音を止めました。");
          }
          if (item.late) {
            park([item], `録音区間を送れません（${message}）。この端末に保存し、あとで送り直します。`);
          } else {
            // 同じ会議の残りも、取り込みとして送る。
            const rest = cap.queue.filter((q) => q.meetingId === item.meetingId);
            cap.queue = cap.queue.filter((q) => q.meetingId !== item.meetingId);
            park([item, ...rest], `会議が録音を受け付けなくなりました（${message}）。未送信の録音はこの端末に保存し、会議の録音として取り込み直します。`);
          }
          void pumpParked();
          continue;
        }
        notify(
          `録音の送信を再試行します（未送信 ${cap.queue.length} 区間はこの端末に保存済み）。マイクは止めていません。 ` + message,
          "warn",
          "upload",
        );
        await waitOrWake(delay);
        delay = Math.min(delay * 2, 10000);
      }
    }
  } finally {
    cap.pumping = false;
    publish();
  }
}

// オフラインからの復帰・タブが前面に戻ったときに、待っている送信をすぐ起こす。
const wakers = new Set<() => void>();
function waitOrWake(ms: number): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(done, ms);
    function done() {
      clearTimeout(timer);
      wakers.delete(done);
      resolve();
    }
    wakers.add(done);
  });
}

function kick() {
  for (const wake of [...wakers]) wake();
  if (cap.queue.length) void pumpUploads();
  if (cap.parked.length) void pumpParked();
  if (cap.captionQueue.length) void pumpCaptions();
}

/** 会議の終了後などに残った区間を、取り込み（X-Import）として送る。受け付けられるまで端末に残す。 */
async function pumpParked() {
  if (cap.parkedPumping || cap.paused || !cap.parked.length) return;
  cap.parkedPumping = true;
  try {
    for (const item of [...cap.parked]) {
      if (cap.paused) break;
      try {
        await request(`/api/meetings/${item.meetingId}/audio`, { method: "POST", headers: audioHeaders(item), raw: item.wav as unknown as BodyInit });
        cap.parked = cap.parked.filter((q) => q !== item);
        forget(item);
      } catch (err) {
        const status = err instanceof ApiError ? err.status : 0;
        if (status === 404) {
          cap.parked = cap.parked.filter((q) => q !== item);
          forget(item);
        } else if (status === 401) {
          cap.paused = true;
          break;
        }
        // それ以外（まとめ中・上限・通信）は残して次の機会に送る。
      }
    }
  } finally {
    cap.parkedPumping = false;
    if (!cap.parked.length) clearNotice("upload-drop");
    else notify(`この端末に、まだ送れていない録音が ${cap.parked.length} 区間あります。自動で送り直しています。`, "warn", "upload-drop");
    publish();
  }
}

/** Chrome の確定文をサーバへ残す。送れるまで端末に残す。 */
export function saveCaption(meetingId: number, startSec: number, text: string) {
  const userId = cap.userId ?? currentUserId;
  if (userId == null) return;
  const base = { meetingId, userId, startSec, text };
  const saved = vault
    .putCaption({ ...base, createdAt: Date.now() })
    .then((id) => {
      vaultWorks();
      return id;
    })
    .catch((err) => {
      vaultFailed(err);
      return null;
    });
  cap.captionQueue.push({ ...base, saved });
  publish();
  void pumpCaptions();
}

async function pumpCaptions() {
  if (cap.captionPumping || cap.paused) return;
  cap.captionPumping = true;
  let delay = 1000;
  try {
    while (cap.captionQueue.length && !cap.paused) {
      const item = cap.captionQueue[0];
      try {
        await request(`/api/meetings/${item.meetingId}/captions`, { method: "POST", body: { start_sec: item.startSec, text: item.text } });
        done();
        delay = 1000;
        clearNotice("caption");
      } catch (err) {
        const status = err instanceof ApiError ? err.status : 0;
        if (status === 401) {
          cap.paused = true;
          break;
        }
        if (status === 404 || status === 400) {
          // 会議が無い・内容が受け付けられない。送り直しても通らない。
          done();
          continue;
        }
        notify("書き起こしの保存を再試行します（この端末に保存済み）。", "warn", "caption");
        await waitOrWake(delay);
        delay = Math.min(delay * 2, 15000);
      }
      function done() {
        cap.captionQueue.shift();
        void item.saved.then((id) => (id == null ? undefined : vault.deleteCaption(id).catch(() => {})));
        publish();
      }
    }
  } finally {
    cap.captionPumping = false;
    publish();
  }
}

let currentUserId: number | null = null;

async function lockFree(meetingId: number): Promise<boolean> {
  const locks = (navigator as Navigator & { locks?: LockManager }).locks;
  if (!locks?.query) return true;
  try {
    const state = await locks.query();
    return !(state.held || []).some((lock) => lock.name === recLockName(meetingId));
  } catch {
    return true;
  }
}

function recLockName(meetingId: number) {
  return `kikimimi-rec-${meetingId}`;
}

/**
 * ブラウザが落ちる・タブを閉じる前に送れなかった録音と書き起こしを、保管庫から読み戻して送る。
 * 別のタブがいま録音している会議の分は、そのタブに任せる（Web Locks で確かめる）。
 */
async function restoreVault(userId: number) {
  if (cap.restoredFor === userId) return;
  cap.restoredFor = userId;
  void vault.askPersistent();
  let chunks: vault.VaultChunk[] = [];
  let tails: vault.VaultTail[] = [];
  let lines: vault.VaultCaption[] = [];
  try {
    [chunks, tails, lines] = await Promise.all([vault.listChunks(), vault.listTails(), vault.listCaptions()]);
    vaultWorks();
  } catch (err) {
    vaultFailed(err);
    return;
  }
  const busy = new Map<number, boolean>();
  const usable = async (meetingId: number) => {
    if (recordingHere(meetingId)) return false;
    if (!busy.has(meetingId)) busy.set(meetingId, !(await lockFree(meetingId)));
    return !busy.get(meetingId);
  };
  const known = new Set<number>();
  for (const item of [...cap.queue, ...cap.parked]) {
    const id = await item.saved;
    if (id != null) known.add(id);
  }
  let restored = 0;
  let seconds = 0;
  for (const tail of tails) {
    if (tail.userId !== userId || !(await usable(tail.meetingId))) continue;
    const pcm = new Int16Array(tail.pcm);
    if (!pcm.length) {
      await vault.deleteTail(userId, tail.meetingId).catch(() => {});
      continue;
    }
    // 途中保存を区間に変える（保存と tail の削除は同じトランザクション）。
    const wav = vault.int16ToWav(pcm, 16000);
    try {
      const id = await vault.putChunk({ userId, meetingId: tail.meetingId, startMs: tail.startMs, wav, silent: false, createdAt: Date.now() });
      chunks.push({ id, userId, meetingId: tail.meetingId, startMs: tail.startMs, wav, silent: false, createdAt: Date.now() });
    } catch (err) {
      vaultFailed(err);
    }
  }
  const mine = chunks.filter((row) => row.userId === userId && row.id != null && !known.has(row.id));
  mine.sort((a, b) => a.meetingId - b.meetingId || a.startMs - b.startMs);
  for (const row of mine) {
    if (!(await usable(row.meetingId))) continue;
    const item: QueueItem = {
      meetingId: row.meetingId,
      userId,
      startMs: row.startMs,
      wav: new Uint8Array(row.wav),
      silent: row.silent,
      late: row.late,
      saved: Promise.resolve(row.id as number),
    };
    if (row.late) cap.parked.push(item);
    else cap.queue.push(item);
    restored += 1;
    seconds += vault.wavSamples(row.wav) / 16000;
  }
  let captionCount = 0;
  for (const line of lines) {
    if (line.userId !== userId || line.id == null) continue;
    cap.captionQueue.push({ meetingId: line.meetingId, userId, startSec: line.startSec, text: line.text, saved: Promise.resolve(line.id) });
    captionCount += 1;
  }
  if (restored || captionCount) {
    const parts = [];
    if (restored) parts.push(`録音 ${restored} 区間（約 ${Math.round(seconds)} 秒）`);
    if (captionCount) parts.push(`書き起こし ${captionCount} 件`);
    notify(`前回ブラウザを閉じる前に送れなかった${parts.join("と")}を、この端末から送っています。`, "warn", "restore");
  }
  publish();
  kick();
}

/** 同じ利用者でログインし直したら、止めていた送信を再開する。別の利用者の分は端末に残し、送らない。 */
export function resumeForUser(userId: number) {
  currentUserId = userId;
  const foreign = (list: { userId: number }[]) => list.filter((item) => item.userId !== userId).length;
  const count = foreign(cap.queue) + foreign(cap.parked) + foreign(cap.captionQueue);
  if (count) {
    // 別のアカウントの分は送らない。端末の保管庫には残し、その人がログインしたときに送る。
    cap.queue = cap.queue.filter((item) => item.userId === userId);
    cap.parked = cap.parked.filter((item) => item.userId === userId);
    cap.captionQueue = cap.captionQueue.filter((item) => item.userId === userId);
    notify(`別のアカウントの未送信分（${count} 件）は、この端末に残しています。そのアカウントでログインすると送ります。`, "warn", "upload");
  }
  if (cap.capturing && cap.userId !== userId) stopCapturing("別のアカウントでログインしたため、録音を止めました。");
  if (cap.restoredFor !== userId) cap.restoredFor = null;
  cap.paused = false;
  publish();
  kick();
  void restoreVault(userId);
}

/** ログアウト時。録音を止め、未送信の区間も（利用者が破棄を選んだので）端末からも消す。 */
export function resetAll() {
  const userId = cap.userId ?? currentUserId;
  cap.capturing = false;
  cap.queue = [];
  cap.parked = [];
  cap.captionQueue = [];
  cap.buffers = [];
  cap.buffered = 0;
  cap.meetingId = null;
  cap.userId = null;
  cap.paused = false;
  cap.restoredFor = null;
  currentUserId = null;
  resetVad();
  releaseGuards();
  stopStream();
  if (userId != null) void vault.forgetUser(userId).catch(() => {});
  publish();
}

/** 会議を削除したとき。その会議の未送信分を、メモリと端末から消す。 */
export function dropQueue(meetingId: number) {
  const keep = (item: { meetingId: number }) => item.meetingId !== meetingId;
  cap.queue = cap.queue.filter(keep);
  cap.parked = cap.parked.filter(keep);
  cap.captionQueue = cap.captionQueue.filter(keep);
  void vault.forgetMeeting(meetingId).catch(() => {});
  publish();
}

/** 送り切れないまま先へ進むとき。未送信の区間は捨てず、端末に残して取り込みとして送り直す。 */
export function parkQueue() {
  const items = cap.queue;
  cap.queue = [];
  park(items, `未送信の録音 ${items.length} 区間はこの端末に保存しました。つながり次第、会議の録音として取り込みます。`);
}

/** 次に録音を始める位置（16kHz のサンプル数）。サーバが受け取った長さと、端末に残る区間の終わりの遅い方。 */
export function resumeOffset(meetingId: number, serverSeconds: number): number {
  let end = Math.round((serverSeconds || 0) * 16000);
  for (const item of [...cap.queue, ...cap.parked]) {
    if (item.meetingId !== meetingId) continue;
    end = Math.max(end, item.startMs * 16 + Math.max(0, (item.wav.byteLength - 44) / 2));
  }
  if (cap.meetingId === meetingId) end = Math.max(end, cap.sentSamples);
  return end;
}

export function queueLength(): number {
  return cap.queue.length;
}

export function queuedFor(id: number): number {
  return cap.queue.filter((item) => item.meetingId === id).length;
}

export function queueBusy(): boolean {
  return cap.queue.length > 0 || cap.pumping || cap.captionQueue.length > 0;
}

// 未送信の区間（と書き起こし）が無くなるまで待つ。時間内に送り切れなければ false。
export function drainQueue(timeoutMs = 15000): Promise<boolean> {
  kick();
  return new Promise((resolve) => {
    const started = Date.now();
    const timer = setInterval(() => {
      if (!cap.queue.length && !cap.pumping && !cap.captionQueue.length && !cap.captionPumping) {
        clearInterval(timer);
        resolve(true);
      } else if (Date.now() - started > timeoutMs || cap.paused) {
        clearInterval(timer);
        resolve(false);
      }
    }, 50);
  });
}

export function secureContextProblem(): string {
  if (window.isSecureContext && navigator.mediaDevices) return "";
  return "この画面は安全な接続（https または localhost）で開かれていないため、ブラウザがマイクを使わせません。サーバを動かしている PC で http://127.0.0.1 から開くか、README の「LAN の別端末から使う」の手順で https にしてください。";
}

export async function listDevices(): Promise<MediaDeviceInfo[]> {
  if (!navigator.mediaDevices?.enumerateDevices) return [];
  const all = await navigator.mediaDevices.enumerateDevices().catch(() => [] as MediaDeviceInfo[]);
  const inputs = all.filter((device) => device.kind === "audioinput");
  devices.set(inputs);
  return inputs;
}

async function openMic(deviceId: string): Promise<MediaStream> {
  const base = { channelCount: 1, echoCancellation: true, noiseSuppression: true };
  if (deviceId) {
    try {
      return await navigator.mediaDevices.getUserMedia({ audio: Object.assign({ deviceId: { exact: deviceId } }, base) });
    } catch (err) {
      const name = err instanceof DOMException ? err.name : "";
      if (name !== "OverconstrainedError" && name !== "NotFoundError") throw err;
      notify("選んだマイクが見つからないため、既定のマイクを使います。", "warn", "mic");
    }
  }
  return navigator.mediaDevices.getUserMedia({ audio: base });
}

// オンライン会議の相手の声は、会議を開いているタブ（または画面）の音声として取り込む。
async function openTabAudio(): Promise<MediaStream> {
  const stream = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: true, systemAudio: "include" } as DisplayMediaStreamOptions);
  if (!stream.getAudioTracks().length) {
    stream.getTracks().forEach((track) => track.stop());
    throw new Error("共有に音声が含まれていません。共有するタブを選び、「タブの音声も共有する」をオンにしてください。");
  }
  stream.getAudioTracks()[0].addEventListener("ended", () => {
    if (cap.capturing) notify("タブの音声の共有が終わりました。相手側の音声は録音されていません。", "error", "tab");
  });
  return stream;
}

export function setCaptureOptions(mic: string, includeTab: boolean) {
  savePref(PREF_MIC, mic);
  savePref(PREF_TAB, includeTab ? "1" : "0");
  cap.includeTab = includeTab;
}

export function useSavedTabPref() {
  cap.includeTab = pref(PREF_TAB) === "1";
}

export function ensureAudio(): Promise<void> {
  const problem = secureContextProblem();
  if (problem) return Promise.reject(new Error(problem));
  if (cap.stream && cap.ctx) return cap.ctx.resume().then(() => {});
  if (cap.ready) return cap.ready;
  const ready = (async () => {
    const stream = await openMic(pref(PREF_MIC));
    let tabStream: MediaStream | null = null;
    if (cap.includeTab) {
      try {
        tabStream = await openTabAudio();
      } catch (err) {
        stream.getTracks().forEach((track) => track.stop());
        throw err;
      }
    }
    // 16kHz で録れればブラウザが正しく間引く。できない環境では自前で間引く。
    let ctx: AudioContext;
    try {
      ctx = new AudioContext({ sampleRate: 16000 });
    } catch {
      ctx = new AudioContext();
    }
    await ctx.resume();
    cap.sampleRate = ctx.sampleRate;
    cap.stream = stream;
    cap.tabStream = tabStream;
    cap.ctx = ctx;
    cap.deviceLabel = stream.getAudioTracks()[0]?.label || "";
    const mute = ctx.createGain();
    mute.gain.value = 0;
    let node: AudioNode;
    try {
      await ctx.audioWorklet.addModule("/capture-worklet.js");
      const worklet = new AudioWorkletNode(ctx, "capture-processor");
      worklet.port.onmessage = (event) => onPcm(event.data as Float32Array);
      node = worklet;
    } catch {
      const processor = ctx.createScriptProcessor(4096, 1, 1);
      processor.onaudioprocess = (event) => {
        const input = event.inputBuffer.getChannelData(0);
        const copy = new Float32Array(input.length);
        copy.set(input);
        onPcm(copy);
      };
      node = processor;
    }
    const micSource = ctx.createMediaStreamSource(stream);
    micSource.connect(node);
    // 速報字幕（Chrome の音声認識）には、録音と同じ音（マイク＋タブ音声）を渡す。
    const mix = ctx.createMediaStreamDestination();
    micSource.connect(mix);
    if (tabStream) {
      const tabSource = ctx.createMediaStreamSource(tabStream);
      tabSource.connect(node);
      tabSource.connect(mix);
    }
    cap.mixTrack = mix.stream.getAudioTracks()[0] || null;
    node.connect(mute);
    mute.connect(ctx.destination);
    watchMic(stream, ctx);
    if (!navigator.mediaDevices.ondevicechange) {
      navigator.mediaDevices.ondevicechange = () => {
        void listDevices();
        if (!cap.capturing) return;
        notify("マイクの機器が変わりました。音が届いているか確認してください。", "warn", "mic");
      };
    }
    void listDevices();
    publish();
  })();
  cap.ready = ready.catch((err) => {
    cap.ready = null;
    throw err;
  });
  return cap.ready;
}

export function stopStream() {
  if (cap.stream) cap.stream.getTracks().forEach((track) => track.stop());
  if (cap.tabStream) cap.tabStream.getTracks().forEach((track) => track.stop());
  cap.stream = null;
  cap.tabStream = null;
  cap.mixTrack = null;
  cap.ready = null;
  if (cap.ctx) cap.ctx.close().catch(() => {});
  cap.ctx = null;
  cap.level = 0;
  publish();
}

/** 画面が暗くなる・スクリーンセーバーが動くのを止める（録音中だけ）。 */
async function holdWakeLock() {
  const wl = (navigator as Navigator & { wakeLock?: { request: (type: "screen") => Promise<{ release: () => Promise<void>; released?: boolean; addEventListener?: (n: string, f: () => void) => void }> } }).wakeLock;
  if (!wl || !cap.capturing || document.visibilityState !== "visible") return;
  if (cap.wakeLock && !cap.wakeLock.released) return;
  try {
    const lock = await wl.request("screen");
    cap.wakeLock = lock;
    lock.addEventListener?.("release", () => {
      if (cap.wakeLock === lock) cap.wakeLock = null;
    });
    if (!cap.capturing) void releaseWakeLock();
  } catch {
    // 省電力の設定などで断られても、録音は続ける。
  }
}

async function releaseWakeLock() {
  const lock = cap.wakeLock;
  cap.wakeLock = null;
  if (lock && !lock.released) await lock.release().catch(() => {});
}

/** 録音している会議を、ほかのタブに知らせる（復元で横取りさせない）。タブが落ちれば自動で外れる。 */
function holdRecLock(meetingId: number) {
  const locks = (navigator as Navigator & { locks?: LockManager }).locks;
  if (cap.releaseLock) cap.releaseLock();
  cap.releaseLock = null;
  if (!locks?.request) return;
  void locks
    .request(recLockName(meetingId), () => new Promise<void>((resolve) => (cap.releaseLock = resolve)))
    .catch(() => {});
}

function releaseGuards() {
  void releaseWakeLock();
  if (cap.releaseLock) cap.releaseLock();
  cap.releaseLock = null;
}

export function startCapturing(meetingId: number, userId: number, samples: number) {
  cap.meetingId = meetingId;
  cap.userId = userId;
  cap.sentSamples = samples;
  cap.buffers = [];
  cap.buffered = 0;
  cap.paused = false;
  cap.capturing = true;
  cap.lastPcmAt = Date.now();
  cap.micProblem = "";
  cap.recoveries = 0;
  cap.lastRecoveryAt = 0;
  cap.tailAt = Date.now();
  currentUserId = userId;
  resetVad();
  clearNotice("mic");
  holdRecLock(meetingId);
  void holdWakeLock();
  void vault.askPersistent();
  publish();
}

/** 一時停止・終了の前に、録音を止めて残りを区間にし、マイクを離す。 */
export function haltCapture() {
  cutChunk();
  cap.capturing = false;
  releaseGuards();
  stopStream();
  publish();
}

// 音が届かなくなったことを見張る。録音中の画面が無くても動く。
setInterval(() => {
  if (!cap.capturing) return;
  // 省電力・画面オフ・別のウィンドウへの切り替えで音声処理が止められたら、起こし直す。
  if (cap.ctx && cap.ctx.state !== "running" && cap.ctx.state !== "closed") cap.ctx.resume().catch(() => {});
  if (cap.micProblem) {
    const track = cap.stream?.getAudioTracks()[0];
    if (!track || track.readyState === "ended") void recoverMic();
    return;
  }
  const track = cap.stream?.getAudioTracks()[0];
  const trackLost = !!track && (track.readyState === "ended" || track.muted);
  const silent = cap.lastPcmAt && Date.now() - cap.lastPcmAt > 3000;
  if (trackLost || silent) {
    noteMicProblem("マイクから音が届いていません。接続を確認してください。録音の受付は続けています。");
    if (trackLost) void recoverMic();
  }
}, 500);

// つながったら・前面に戻ったら、待たずに送る。画面が戻ったら画面オフ防止も取り直す。
window.addEventListener("online", () => {
  clearNotice("upload");
  kick();
});
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    if (cap.capturing) {
      void holdWakeLock();
      if (cap.ctx && cap.ctx.state !== "running") cap.ctx.resume().catch(() => {});
    }
    kick();
  } else if (cap.capturing) {
    // 裏に回る（画面ロック・別ウィンドウ・最小化）ときは、区切る前の録音もすぐ保存する。
    void writeTail();
  }
});
// タブを閉じる・ページが凍結される直前にも途中保存する。
window.addEventListener("pagehide", () => {
  if (cap.capturing) void writeTail();
});
document.addEventListener("freeze", () => {
  if (cap.capturing) void writeTail();
});
setInterval(() => {
  if (cap.parked.length) void pumpParked();
}, PARKED_RETRY_MS);

// 録音中だけでなく、未送信の区間が残っている間も離脱を確かめる。
window.addEventListener("beforeunload", (event) => {
  if (cap.capturing || cap.queue.length || cap.pumping || cap.captionQueue.length) {
    event.preventDefault();
    event.returnValue = "";
  }
});

/** 開発・検証用に、画面の外から録音の状態を見られるようにする。 */
(window as unknown as { __recorder: typeof cap }).__recorder = cap;
