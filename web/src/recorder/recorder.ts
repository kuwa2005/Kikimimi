// 録音ループはネットワークを待たない。
// 音声スレッドが溜めたサンプルを、無音の切れ目（長くても 15 秒）で区切って送信待ちへ載せるだけ。
// 文字起こし・回答・ナレッジ検索はサーバの後段で進み、マイクは止めない。
// React の外に置き、画面部品のマウント・アンマウント（StrictMode の二重実行を含む）に左右されない。

import { ApiError, limitText, request } from "../api/client";
import type { UsageItem } from "../api/types";
import { createStore } from "../lib/store";
import { pref, PREF_MIC, PREF_TAB, savePref } from "../lib/prefs";
import { sleep } from "../lib/format";
import { notify, clearNotice } from "../state/notices";

const CHUNK = {
  min: 4, // 秒。これより短い区切りは作らない
  max: 15, // 秒。話し続けていてもここで区切る
  silence: 0.6, // 秒。この長さの無音で区切る
  frame: 0.02, // 秒。無音判定の単位
  minSpeech: 0.3, // 秒。区間内の発話がこれ未満なら「無音」として文字起こしに回さない
};

type QueueItem = { meetingId: number; userId: number; startMs: number; wav: Uint8Array; silent: boolean };

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
  pumping: false,
  paused: false, // ログインが切れた間は送らない
  level: 0,
  ready: null as Promise<void> | null,
  lastPcmAt: 0,
  micProblem: "",
  recovering: false,
  recoveries: 0,
  deviceLabel: "",
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
  cap.capturing = false;
  cap.buffers = [];
  cap.buffered = 0;
  resetVad();
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
  if (cap.recovering || cap.recoveries >= 1) return;
  cap.recovering = true;
  cap.recoveries += 1;
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
  cap.queue.push({ meetingId: cap.meetingId, userId: cap.userId, startMs, wav: encodeWav(down, 16000), silent });
  publish();
  void pumpUploads();
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
          headers: {
            "Content-Type": "audio/wav",
            "X-Start-Ms": String(item.startMs),
            "X-Silent": item.silent ? "1" : "0",
          },
          raw: item.wav as unknown as BodyInit,
        });
        cap.queue.shift();
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
            `ログインが切れたため、録音の送信を止めています（未送信 ${cap.queue.length} 区間）。同じアカウントでログインし直すと続きを送ります。`,
            "error",
            "upload",
          );
          break;
        }
        if (status === 403 && err instanceof ApiError && err.code === "limit_exceeded" && err.limit) {
          // サービスの上限。送り直しても通らないので、この会議の録音を止め、区間は捨てる。
          const dropped = cap.queue.filter((q) => q.meetingId === item.meetingId).length;
          cap.queue = cap.queue.filter((q) => q.meetingId !== item.meetingId);
          notify(`${limitText(err.limit)}未送信の ${dropped} 区間は保存されませんでした。`, "error", "limit");
          if (recordingHere(item.meetingId)) stopCapturing("上限に達したため、録音を止めました。");
          limitHandler(item.meetingId, err.limit);
          publish();
          continue;
        }
        if (status >= 400 && status < 500 && status !== 408 && status !== 429) {
          // 会議が終了・削除されたなど、送り直しても受け付けられない区間は捨てる。
          cap.queue.shift();
          notify(`録音区間を送れなかったため破棄しました: ${message}`, "error", "upload-drop");
          if ((status === 409 || status === 404) && recordingHere(item.meetingId)) {
            stopCapturing("この会議は録音を受け付けなくなったため、録音を止めました。");
          }
          publish();
          continue;
        }
        notify("録音の送信を再試行します。マイクは止めていません。 " + message, "warn", "upload");
        await sleep(delay);
        delay = Math.min(delay * 2, 10000);
      }
    }
  } finally {
    cap.pumping = false;
    publish();
  }
}

/** 同じ利用者でログインし直したら、止めていた送信を再開する。別の利用者なら破棄する。 */
export function resumeForUser(userId: number) {
  const foreign = cap.queue.filter((item) => item.userId !== userId);
  if (foreign.length) {
    cap.queue = cap.queue.filter((item) => item.userId === userId);
    notify(`別のアカウントの未送信録音 ${foreign.length} 区間を破棄しました。`, "warn", "upload");
  }
  if (cap.capturing && cap.userId !== userId) stopCapturing("別のアカウントでログインしたため、録音を止めました。");
  cap.paused = false;
  publish();
  if (cap.queue.length) void pumpUploads();
}

/** ログアウト時。録音を止め、未送信の区間も捨てる。 */
export function resetAll() {
  cap.capturing = false;
  cap.queue = [];
  cap.buffers = [];
  cap.buffered = 0;
  cap.meetingId = null;
  cap.userId = null;
  cap.paused = false;
  resetVad();
  stopStream();
  publish();
}

export function dropQueue(meetingId?: number) {
  cap.queue = meetingId == null ? [] : cap.queue.filter((item) => item.meetingId !== meetingId);
  publish();
}

export function queueLength(): number {
  return cap.queue.length;
}

export function queuedFor(id: number): number {
  return cap.queue.filter((item) => item.meetingId === id).length;
}

export function queueBusy(): boolean {
  return cap.queue.length > 0 || cap.pumping;
}

// 未送信の区間が無くなるまで待つ。時間内に送り切れなければ false。
export function drainQueue(timeoutMs = 15000): Promise<boolean> {
  return new Promise((resolve) => {
    const started = Date.now();
    const timer = setInterval(() => {
      if (!cap.queue.length && !cap.pumping) {
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
  resetVad();
  clearNotice("mic");
  publish();
}

/** 一時停止・終了の前に、録音を止めて残りを区間にし、マイクを離す。 */
export function haltCapture() {
  cap.capturing = false;
  cutChunk();
  stopStream();
  publish();
}

// 音が届かなくなったことを見張る。録音中の画面が無くても動く。
setInterval(() => {
  if (!cap.capturing || cap.micProblem) return;
  const track = cap.stream?.getAudioTracks()[0];
  const trackLost = !!track && (track.readyState === "ended" || track.muted);
  const silent = cap.lastPcmAt && Date.now() - cap.lastPcmAt > 3000;
  if (trackLost || silent) {
    noteMicProblem("マイクから音が届いていません。接続を確認してください。録音の受付は続けています。");
    if (trackLost) void recoverMic();
  }
}, 500);

// 録音中だけでなく、未送信の区間が残っている間も離脱を確かめる。
window.addEventListener("beforeunload", (event) => {
  if (cap.capturing || cap.queue.length || cap.pumping) {
    event.preventDefault();
    event.returnValue = "";
  }
});

/** 開発・検証用に、画面の外から録音の状態を見られるようにする。 */
(window as unknown as { __recorder: typeof cap }).__recorder = cap;
