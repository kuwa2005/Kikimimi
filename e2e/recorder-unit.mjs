#!/usr/bin/env node
// 録音部（web/src/recorder/recorder.ts）の回帰試験。ブラウザ不要・ネットワーク不要。
//
//   (cd web && npm ci) && node e2e/recorder-unit.mjs
//
// web/node_modules にある rolldown（vite の依存）で recorder.ts を Node 向けに 1 ファイルへまとめ、
// 偽の AudioContext / マイク / document / navigator の上で実際のコードを動かして、
// 「確実に録音を残す」ための不変条件を確かめる。
//   1. プリロール: 新規開始（samples=0）だけ直前の音を頭に付け、最初の区間は startMs=0。再開では付けない。stopStream で消える。
//   2. 区切り: WAV ヘッダ、resample(16000) は素通し、発話→無音 0.6 秒で区切る位置、最短 4 秒・最長 15 秒、無音区間の印。
//   3. 位置: 区間の startMs は単調増加で、前の区間の終わりとぴったり接する（重なり・隙間なし）。
//   4. タップ待ち: AudioContext.resume() が終わらない環境でも ensureAudio は約 1.5 秒で戻り、タップの案内を出し、
//      タップで動き出すと案内と待ち受けが消える。
// 製品コードは変更しない（試験用に resample だけを export に足して束ねる）。

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const WEB = join(ROOT, "web");
const RECORDER = join(WEB, "src/recorder/recorder.ts");
const NOTICES = join(WEB, "src/state/notices.ts");

// ---------------------------------------------------------------- 試験の道具
let failures = 0;
let passes = 0;
function check(name, ok, detail = "") {
  if (ok) {
    passes += 1;
    console.log(`  PASS ${name}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${name}${detail ? " — " + detail : ""}`);
  }
}
const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------- 偽のブラウザ
function makeTarget() {
  const map = new Map();
  return {
    listeners: map,
    addEventListener(name, fn) {
      if (!map.has(name)) map.set(name, new Set());
      map.get(name).add(fn);
    },
    removeEventListener(name, fn) {
      map.get(name)?.delete(fn);
    },
    count(name) {
      return map.get(name)?.size || 0;
    },
    dispatch(name, event = {}) {
      for (const fn of [...(map.get(name) || [])]) fn(event);
    },
  };
}

const storage = new Map();
const fakeDocument = Object.assign(makeTarget(), { visibilityState: "visible" });
const fakeWindow = makeTarget();

// AudioContext の振る舞い。"running": すぐ動く。"locked": タップの処理の中でしか resume できない（自動再生の制限）。
const audio = { mode: "running", inGesture: false, contexts: [], worklets: [] };

const param = () => ({ value: 0, setValueAtTime() {}, setTargetAtTime() {} });
const node = (extra = {}) => Object.assign({ connect() {}, disconnect() {} }, extra);

function makeTrack(deviceId = "mic-1") {
  const t = makeTarget();
  return Object.assign(t, {
    kind: "audio",
    label: "Fake Mic",
    readyState: "live",
    muted: false,
    getSettings: () => ({ deviceId }),
    applyConstraints: async () => {},
    stop() {
      this.readyState = "ended";
    },
  });
}
function makeStream() {
  const track = makeTrack();
  return { getAudioTracks: () => [track], getTracks: () => [track] };
}

class FakeAudioContext {
  constructor(opts = {}) {
    this.sampleRate = opts.sampleRate || 48000;
    this.state = audio.mode === "running" ? "running" : "suspended";
    this.currentTime = 0;
    this.onstatechange = null;
    this.destination = node();
    this.audioWorklet = { addModule: async () => {} };
    audio.contexts.push(this);
  }
  setState(s) {
    this.state = s;
    this.onstatechange?.();
  }
  resume() {
    if (this.state === "closed") return Promise.reject(new Error("closed"));
    if (audio.mode === "running" || audio.inGesture) {
      this.setState("running");
      return Promise.resolve();
    }
    return new Promise(() => {}); // タップがあるまで終わらない
  }
  close() {
    this.state = "closed";
    return Promise.resolve();
  }
  createMediaStreamSource() {
    return node();
  }
  createGain() {
    return node({ gain: param() });
  }
  createBiquadFilter() {
    return node({ type: "", frequency: param(), Q: param() });
  }
  createDynamicsCompressor() {
    return node({ threshold: param(), knee: param(), ratio: param(), attack: param(), release: param() });
  }
  createMediaStreamDestination() {
    return node({ stream: makeStream() });
  }
  createScriptProcessor() {
    return node({ onaudioprocess: null });
  }
}
class FakeAudioWorkletNode {
  constructor() {
    this.port = { onmessage: null };
    audio.worklets.push(this);
  }
  connect() {}
  disconnect() {}
}

Object.assign(globalThis, {
  window: globalThis,
  document: fakeDocument,
  isSecureContext: true,
  addEventListener: fakeWindow.addEventListener,
  removeEventListener: fakeWindow.removeEventListener,
  localStorage: {
    getItem: (k) => (storage.has(k) ? storage.get(k) : null),
    setItem: (k, v) => storage.set(k, String(v)),
    removeItem: (k) => storage.delete(k),
  },
  sessionStorage: { getItem: () => null, setItem() {}, removeItem() {} },
  matchMedia: () => ({ matches: false, addEventListener() {} }),
  AudioContext: FakeAudioContext,
  AudioWorkletNode: FakeAudioWorkletNode,
  // 回線が死んでいる状態（応答が返らない）。送信待ちの区間はキューに残るので、区切りをそのまま調べられる。
  fetch: () => new Promise(() => {}),
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: {
    userAgent: "Mozilla/5.0 (X11; Linux x86_64) Chrome/140 Node-Test",
    maxTouchPoints: 0,
    mediaDevices: {
      getUserMedia: async () => makeStream(),
      enumerateDevices: async () => [],
      ondevicechange: null,
    },
  },
});

// ---------------------------------------------------------------- recorder.ts を束ねる
async function bundle() {
  let rolldown;
  try {
    ({ rolldown } = await import(pathToFileURL(join(WEB, "node_modules/rolldown/dist/index.mjs")).href));
  } catch {
    ({ rolldown } = await import(pathToFileURL(join(WEB, "node_modules/rolldown/index.js")).href).catch(() => ({})));
  }
  if (!rolldown) throw new Error("web/node_modules/rolldown が見つかりません。先に (cd web && npm ci) を実行してください。");
  const ENTRY = "\0recorder-unit-entry";
  const build = await rolldown({
    input: ENTRY,
    platform: "node",
    logLevel: "silent",
    plugins: [
      {
        name: "recorder-unit",
        resolveId(id) {
          return id === ENTRY ? ENTRY : null;
        },
        load(id) {
          if (id !== ENTRY) return null;
          return `export * from ${JSON.stringify(RECORDER)};\nexport { notices } from ${JSON.stringify(NOTICES)};\n`;
        },
        transform(code, id) {
          // 内部関数のうち、試験で直接呼ぶものだけを足す（中身は変えない）。
          if (id === RECORDER) return { code: code + "\nexport { resample as __resample, resumeWithin as __resumeWithin };\n" };
          return null;
        },
      },
    ],
  });
  const { output } = await build.generate({ format: "esm" });
  await build.close?.();
  const dir = mkdtempSync(join(tmpdir(), "recorder-unit-"));
  const file = join(dir, "recorder.bundle.mjs");
  writeFileSync(file, output[0].code);
  try {
    return await import(pathToFileURL(file).href);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const R = await bundle();
const cap = globalThis.__recorder;
const RATE = 16000;

// ---------------------------------------------------------------- 音の部品
function tone(seconds, amp = 0.3, rate = RATE) {
  const n = Math.round(seconds * rate);
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) out[i] = amp * Math.sin((2 * Math.PI * 440 * i) / rate);
  return out;
}
const silence = (seconds, rate = RATE) => new Float32Array(Math.round(seconds * rate));
/** マイクから音が届いたことにする（AudioWorklet が 20ms ずつ送る）。 */
function feed(samples, block = 320) {
  const w = audio.worklets.at(-1);
  for (let i = 0; i < samples.length; i += block) w.port.onmessage({ data: samples.slice(i, i + block) });
}
function wavInfo(wav) {
  const v = new DataView(wav.buffer, wav.byteOffset, wav.byteLength);
  const tag = (o) => String.fromCharCode(...wav.subarray(o, o + 4));
  return {
    riff: tag(0),
    size: v.getUint32(4, true),
    wave: tag(8),
    fmt: tag(12),
    fmtLen: v.getUint32(16, true),
    pcm: v.getUint16(20, true),
    channels: v.getUint16(22, true),
    rate: v.getUint32(24, true),
    byteRate: v.getUint32(28, true),
    align: v.getUint16(32, true),
    bits: v.getUint16(34, true),
    data: tag(36),
    dataLen: v.getUint32(40, true),
    sample: (i) => v.getInt16(44 + i * 2, true),
    samples: v.getUint32(40, true) / 2,
  };
}
const chunksOf = (meetingId) => cap.queue.filter((q) => q.meetingId === meetingId);
const durMs = (q) => (wavInfo(q.wav).samples / RATE) * 1000;
const noticeIds = () => R.notices.get().map((n) => n.id);

async function freshMic(mode = "running") {
  R.stopStream();
  audio.mode = mode;
  await R.ensureAudio();
}

// ================================================================ 1. プリロール
console.log("1. PRE-ROLL");
{
  await freshMic();
  check("AudioContext は 16kHz で開く", cap.sampleRate === RATE, `sampleRate=${cap.sampleRate}`);
  feed(tone(10));
  check("録音前は直近 6 秒前後だけを保持する", cap.prerolled >= 6 * RATE && cap.prerolled <= 6 * RATE + 320, `prerolled=${cap.prerolled}`);
  check("録音前は区間を作らない", cap.queue.length === 0 && cap.buffered === 0);

  R.stopStream();
  check("stopStream でプリロールが消える", cap.prerolled === 0 && cap.preroll.length === 0);

  await freshMic();
  const before = tone(4, 0.25);
  feed(before);
  check("押す前の 4 秒がプリロールに入る", cap.prerolled === 4 * RATE, `prerolled=${cap.prerolled}`);
  R.startCapturing(101, 1, 0);
  check("新規開始（samples=0）でプリロールを頭に付ける", cap.buffered === 4 * RATE, `buffered=${cap.buffered}`);
  check("付けたあとプリロールは空になる", cap.prerolled === 0 && cap.preroll.length === 0);
  feed(tone(1, 0.25));
  R.cutChunk();
  const first = chunksOf(101)[0];
  check("最初の区間の startMs は 0", first && first.startMs === 0, `startMs=${first?.startMs}`);
  const info = first && wavInfo(first.wav);
  check("最初の区間はプリロール 4 秒＋押したあと 1 秒", info && info.samples === 5 * RATE, `samples=${info?.samples}`);
  const expect0 = Math.round(before[100] * 0x7fff);
  check("区間の頭はプリロールの音そのもの", info && Math.abs(info.sample(100) - expect0) <= 1, `got=${info?.sample(100)} want=${expect0}`);
  check("プリロール入りの区間は発話あり（silent=false）", first && first.silent === false);

  // 再開（samples>0）: 前の録音位置の続きなので、付けると重なる。付けてはいけない。
  R.haltCapture();
  await freshMic();
  feed(tone(2));
  const resumeAt = 5 * RATE;
  R.startCapturing(101, 1, resumeAt);
  check("再開（samples>0）ではプリロールを付けない", cap.buffered === 0, `buffered=${cap.buffered}`);
  check("再開でもプリロールは捨てる", cap.prerolled === 0);
  feed(tone(2));
  R.cutChunk();
  const resumed = chunksOf(101)[1];
  check("再開後の区間は続きの位置（5000ms）から", resumed && resumed.startMs === 5000, `startMs=${resumed?.startMs}`);
  check("再開後の区間は押したあとの音だけ（2 秒）", resumed && wavInfo(resumed.wav).samples === 2 * RATE);
  R.haltCapture();
}

// ================================================================ 2. 区切り
console.log("2. CHUNKING");
{
  const wav = R.encodeWav(new Float32Array([0, 0.5, -0.5, 1.5, -2]), RATE);
  const h = wavInfo(wav);
  check("WAV: RIFF/WAVE/fmt/data の並び", h.riff === "RIFF" && h.wave === "WAVE" && h.fmt === "fmt " && h.data === "data");
  check("WAV: 長さ（RIFF=36+data, data=2*n, 全体=44+data）", h.size === 36 + 10 && h.dataLen === 10 && wav.byteLength === 54);
  check(
    "WAV: PCM・モノラル・16kHz・16bit",
    h.fmtLen === 16 && h.pcm === 1 && h.channels === 1 && h.rate === RATE && h.byteRate === RATE * 2 && h.align === 2 && h.bits === 16,
  );
  check("WAV: ±1 で頭打ちし、符号付き 16bit に変換", h.sample(0) === 0 && h.sample(1) === 16383 && h.sample(2) === -16384 && h.sample(3) === 32767 && h.sample(4) === -32768);

  const x = tone(0.5);
  check("resample(16000) は素通し（同じ配列）", R.__resample(x, 16000) === x);
  const y = R.__resample(tone(1, 0.3, 48000), 48000);
  check("resample(48000) は 1/3 の長さの 16kHz", y.length === RATE, `len=${y.length}`);

  // 発話 5 秒 → 無音: 無音が 0.6 秒続いた所で区切る（5.6 秒）。そのまま無音なら最長 15 秒で区切り、無音の印が付く。
  await freshMic();
  R.startCapturing(202, 1, 0);
  feed(tone(5));
  check("話している間（< 15 秒）は区切らない", chunksOf(202).length === 0);
  feed(silence(0.58));
  check("無音 0.58 秒ではまだ区切らない", chunksOf(202).length === 0);
  feed(silence(0.02));
  let c = chunksOf(202);
  check("発話のあと無音 0.6 秒で区切る", c.length === 1, `chunks=${c.length}`);
  check("区切り位置は 5.6 秒（発話 5 秒＋無音 0.6 秒）", c[0] && wavInfo(c[0].wav).samples === 5.6 * RATE, `samples=${c[0] && wavInfo(c[0].wav).samples}`);
  check("発話を含む区間は silent=false", c[0]?.silent === false);
  feed(silence(15));
  c = chunksOf(202);
  check("黙ったままなら最長 15 秒で区切る", c.length === 2 && wavInfo(c[1].wav).samples === 15 * RATE, `chunks=${c.length} samples=${c[1] && wavInfo(c[1].wav).samples}`);
  check("無音だけの区間は silent=true", c[1]?.silent === true);
  // 短い発話のあとの無音では、最短 4 秒までは区切らない。
  feed(tone(1));
  feed(silence(2.5));
  check("最短 4 秒未満では区切らない", chunksOf(202).length === 2);
  feed(silence(0.5));
  c = chunksOf(202);
  check("4 秒に達した所で区切る", c.length === 3 && wavInfo(c[2].wav).samples === 4 * RATE, `chunks=${c.length} samples=${c[2] && wavInfo(c[2].wav).samples}`);
  // 話し続けても 15 秒で区切る。
  feed(tone(16));
  c = chunksOf(202);
  check("話し続けても 15 秒で区切る", c.length === 4 && wavInfo(c[3].wav).samples === 15 * RATE);
  // 中途半端な長さのブロック（Firefox の ScriptProcessor 4096 など）でも位置はずれない。
  feed(tone(3.3), 4096);
  feed(silence(1.0), 4096);
  R.cutChunk();
  R.haltCapture();
}

// ================================================================ 3. 位置
console.log("3. POSITION");
{
  for (const id of [101, 202]) {
    const list = chunksOf(id);
    let ok = list.length >= 2;
    let detail = "";
    for (let i = 1; i < list.length; i++) {
      const prev = list[i - 1];
      const end = prev.startMs + durMs(prev);
      if (!(list[i].startMs > prev.startMs)) {
        ok = false;
        detail = `#${i} startMs ${list[i].startMs} <= ${prev.startMs}`;
      } else if (Math.abs(list[i].startMs - end) > 1) {
        ok = false;
        detail = `#${i} startMs ${list[i].startMs} != 前の終わり ${end}`;
      }
    }
    check(`会議 ${id}: ${list.length} 区間の startMs が単調増加・重なりも隙間もない`, ok, detail);
  }
  const total = chunksOf(202).reduce((s, q) => s + wavInfo(q.wav).samples, 0);
  check("会議 202: 送った音の合計 = 録った音の合計（取りこぼしなし）", total === cap.sentSamples, `total=${total} sent=${cap.sentSamples}`);
}

// ================================================================ 4. タップ待ち
console.log("4. RESUME-GESTURE");
{
  R.stopStream();
  audio.mode = "locked";
  let t0 = Date.now();
  await R.ensureAudio();
  let took = Date.now() - t0;
  const ctx = audio.contexts.at(-1);
  check("resume が終わらなくても ensureAudio は約 1.5 秒で戻る", took >= 1400 && took < 2500, `${took}ms`);
  check("戻った時点で AudioContext は止まったまま", ctx.state === "suspended");
  check("タップの案内（tap）を出す", noticeIds().includes("tap"));
  check("pointerdown / touchend / keydown の待ち受けを登録する", ["pointerdown", "touchend", "keydown"].every((n) => fakeDocument.count(n) === 1));
  // 止まっていても録音は開始できる（会議は作られ、音が届き始めたら録る）。
  R.startCapturing(303, 1, 0);
  check("止まったままでも録音の開始は進む", cap.capturing === true && cap.meetingId === 303);

  audio.inGesture = true;
  fakeDocument.dispatch("pointerdown");
  audio.inGesture = false;
  await tick(0);
  check("タップで AudioContext が動き出す", ctx.state === "running");
  check("動き出したら案内を消す", !noticeIds().includes("tap"));
  check("動き出したら待ち受けを外す", ["pointerdown", "touchend", "keydown"].every((n) => fakeDocument.count(n) === 0));

  // iOS の裏から戻った直後など、録音中に止まった場合: 早期 return の経路も 1.5 秒で戻る。
  ctx.state = "suspended";
  t0 = Date.now();
  await R.ensureAudio();
  took = Date.now() - t0;
  check("開いたマイクで止まった場合も ensureAudio は約 1.5 秒で戻る", took >= 1400 && took < 2500, `${took}ms`);
  check("そのときも案内と待ち受けを出す", noticeIds().includes("tap") && fakeDocument.count("pointerdown") === 1);
  R.haltCapture();
  check("停止で待ち受けと案内を片付ける", fakeDocument.count("pointerdown") === 0 && !noticeIds().includes("tap"));

  // 直接呼んだ場合（resumeWithin）
  audio.mode = "locked";
  const lone = new FakeAudioContext();
  t0 = Date.now();
  await R.__resumeWithin(lone);
  took = Date.now() - t0;
  check("resumeWithin 単体でも 1.5 秒前後で戻る", took >= 1400 && took < 2500, `${took}ms`);
  audio.mode = "running";
  const ok = new FakeAudioContext();
  t0 = Date.now();
  await R.__resumeWithin(ok);
  check("動いている AudioContext ではすぐ戻る", Date.now() - t0 < 50);
}

console.log(`\n${passes} passed, ${failures} failed`);
process.exit(failures ? 1 : 0);
