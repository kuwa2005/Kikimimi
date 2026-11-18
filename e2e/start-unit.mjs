#!/usr/bin/env node
// 録音開始の経路（web/src/state/actions.tsx の beginMeeting と、web/src/state/offline.ts）の回帰試験。
// ブラウザ不要・ネットワーク不要。
//
//   (cd web && npm ci) && node e2e/start-unit.mjs
//
// actions.tsx・offline.ts・api/client.ts は本物を使う（fetch だけ偽物）。マイク（recorder）・保管庫（vault）・
// 画面まわり（router / notices / session / draft など）は、呼ばれた内容を記録する偽物に差し替える。
// 確かめること:
//   F1     開始系 API が 5xx・回線断・タイムアウトなら端末だけの録音（beginOfflineMeeting）に切り替わり、
//          4xx（401/403/404）では切り替わらない（マイクを離してエラーを返す）。
//   BUDGET settings → 会議作成 → start の合計の待ちが約 4 秒で打ち切られる（呼び出しごとに 4 秒ではない）。
//   F4     beginMeeting を同時に 2 回呼んでも 1 つの開始にまとまり、終わればまた開始できる。
//   F3     端末の保管庫（IndexedDB）が応答しなくても、録音（startCapturing）は始まる。
// 製品コードは変更しない。

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const WEB = join(ROOT, "web");
const SRC = join(WEB, "src");
const ACTIONS = join(SRC, "state/actions.tsx");
const OFFLINE = join(SRC, "state/offline.ts");
const CLIENT = join(SRC, "api/client.ts");
const REAL = new Set([ACTIONS, OFFLINE, CLIENT]);

// ---------------------------------------------------------------- 試験の道具
let failures = 0;
let passes = 0;
const known = [];
function check(name, ok, detail = "") {
  if (ok) {
    passes += 1;
    console.log(`  PASS ${name}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${name}${detail ? " — " + detail : ""}`);
  }
}
/** 製品の既知の不具合。試験の合否には数えず、毎回はっきり表示する（直ったら XPASS と出る）。 */
function knownBug(name, ok, detail = "") {
  if (ok) console.log(`  XPASS ${name}（既知の不具合が直っている。knownBug から check に移すこと）`);
  else {
    known.push(name);
    console.log(`  KNOWN-BUG ${name}${detail ? " — " + detail : ""}`);
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** promise が ms 以内に終わるか（終わらなければ "pending"）。 */
function settleWithin(promise, ms) {
  return Promise.race([
    promise.then(
      () => "resolved",
      (err) => ({ rejected: err }),
    ),
    sleep(ms).then(() => "pending"),
  ]);
}

// ---------------------------------------------------------------- 偽物（呼び出しの記録）
const log = [];
const S = {
  // recorder
  recorderState: { get: () => ({ capturing: false, meetingId: null }) },
  micMatchesSelection: () => false,
  micOnly: () => log.push(["micOnly"]),
  stopStream: () => log.push(["stopStream"]),
  ensureAudio: async () => {
    log.push(["ensureAudio"]);
    await sleep(S.ensureAudioDelay || 0);
  },
  ensureAudioDelay: 0,
  startCapturing: (id, userId, samples) => log.push(["startCapturing", id, userId, samples]),
  kickUploads: () => log.push(["kickUploads"]),
  setLocalBinder: () => {},
  // vault
  putMeeting: async (m) => {
    log.push(["putMeeting", m.localId]);
    if (S.vaultHang) return new Promise(() => {});
  },
  listMeetings: async () => (S.vaultHang ? new Promise(() => {}) : []),
  vaultHang: false,
  // 画面・状態
  currentUser: () => ({ id: 7 }),
  isOffline: () => false,
  refreshSession: async () => true,
  notify: (message, kind, key) => log.push(["notify", kind, key, message]),
  navigate: (hash) => log.push(["navigate", hash]),
  clearForm: () => {},
  formKey: (id) => `form:${id}`,
  setSetupReturn: () => {},
  setDetail: (d) => log.push(["setDetail", d?.meeting?.id]),
  emptyForm: () => form(),
  nowTitle: () => "2026-11-18 10:00",
  createStore: (value) => {
    let v = value;
    return { get: () => v, set: (n) => (v = n), subscribe: () => () => {} };
  },
};
globalThis.__S = S;

// navigator.onLine が無いと beginMeeting は最初からオフライン扱いになる。
Object.defineProperty(globalThis, "navigator", { value: { onLine: true, userAgent: "node" }, configurable: true });
// offline.ts は読み込み時に "online" などを待ち受ける（今回の試験では発火させない）。
globalThis.window = { addEventListener() {}, removeEventListener() {} };
globalThis.document = { addEventListener() {}, removeEventListener() {}, visibilityState: "visible" };

// ---------------------------------------------------------------- 偽の fetch（サーバ）
// routes[path] = (init) => Response | Promise<Response>。signal（タイムアウト）には従う。
const routes = {};
const calls = [];
const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const detail = (id) => ({ meeting: { id, status: "recording", speech: "chrome", duration_sec: 0 } });
function slow(ms, make) {
  return (init) =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => resolve(make()), ms);
      init.signal?.addEventListener("abort", () => {
        clearTimeout(timer);
        reject(new DOMException("The operation was aborted.", "TimeoutError"));
      });
    });
}
const hang = () => slow(1e9, () => json({}));
function okRoutes() {
  for (const k of Object.keys(routes)) delete routes[k];
  routes["GET /api/settings"] = () => json({ provider: "gemini", api_key_set: true });
  routes["POST /api/meetings"] = () => json(detail(41));
  routes["POST /api/meetings/41/start"] = () => json(detail(41));
}
globalThis.fetch = async (path, init = {}) => {
  const key = `${init.method || "GET"} ${path}`;
  calls.push({ key, at: Date.now() });
  log.push(["fetch", key]);
  const handler = routes[key];
  if (!handler) throw new TypeError(`unexpected fetch ${key}`);
  return handler(init);
};

function reset() {
  log.length = 0;
  calls.length = 0;
  S.vaultHang = false;
  S.ensureAudioDelay = 0;
  okRoutes();
}
const count = (name) => log.filter((e) => e[0] === name).length;
const captured = () => log.filter((e) => e[0] === "startCapturing");
function form() {
  return { title: "", questions: [], minutes: [], participants: "", folderId: "", templateId: "", speech: "chrome", transcriptOnly: false };
}

// ---------------------------------------------------------------- actions.tsx を束ねる
function exportNames(file) {
  const code = readFileSync(file, "utf8");
  const names = new Set();
  for (const m of code.matchAll(/export\s+(?:async\s+)?(?:function\*?|const|let|var|class)\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of code.matchAll(/export\s*\{([^}]*)\}/g)) {
    for (const part of m[1].split(",")) {
      const name = part.trim().split(/\s+as\s+/).pop();
      if (name && !name.startsWith("type ")) names.add(name);
    }
  }
  return [...names];
}
function stubSource(names) {
  // 呼び出し・プロパティ参照のたびに globalThis.__S[name] を見る（試験ごとに差し替えられる）。
  const lines = names.map(
    (n) =>
      `export const ${n} = new Proxy(function () {}, {` +
      ` apply: (_t, self, args) => target(${JSON.stringify(n)}).apply(self, args),` +
      ` get: (_t, p) => target(${JSON.stringify(n)})[p] });`,
  );
  return `function target(n) { const v = globalThis.__S[n]; if (v === undefined) throw new Error("start-unit: 偽物が無い: " + n); return v; }\n${lines.join("\n")}\n`;
}

async function bundle() {
  let rolldown;
  try {
    ({ rolldown } = await import(pathToFileURL(join(WEB, "node_modules/rolldown/dist/index.mjs")).href));
  } catch {
    ({ rolldown } = await import(pathToFileURL(join(WEB, "node_modules/rolldown/index.js")).href).catch(() => ({})));
  }
  if (!rolldown) throw new Error("web/node_modules/rolldown が見つかりません。先に (cd web && npm ci) を実行してください。");
  const ENTRY = "\0start-unit-entry";
  const STUB = "\0stub:";
  const resolveLocal = (source, importer) => {
    const base = resolve(dirname(importer), source);
    for (const ext of ["", ".ts", ".tsx"]) {
      try {
        readFileSync(base + ext);
        return base + ext;
      } catch {}
    }
    return null;
  };
  const build = await rolldown({
    input: ENTRY,
    platform: "node",
    logLevel: "silent",
    plugins: [
      {
        name: "start-unit",
        resolveId(source, importer) {
          if (source === ENTRY) return ENTRY;
          if (!importer || importer === ENTRY || source.startsWith("node:") || importer.startsWith("\0rolldown")) return null;
          if (source.startsWith(".")) {
            const file = resolveLocal(source, importer);
            if (file && REAL.has(file)) return file;
            if (file) return STUB + file;
          }
          // react/jsx-runtime など（drainOrAsk のダイアログ。今回の試験では呼ばない）。
          return STUB + "pkg:" + source;
        },
        load(id) {
          if (id === ENTRY) return `export * from ${JSON.stringify(ACTIONS)};\n`;
          if (!id.startsWith(STUB)) return null;
          const target = id.slice(STUB.length);
          if (target.startsWith("pkg:")) return "export const jsx = () => null, jsxs = () => null, Fragment = null;\nexport default {};\n";
          return stubSource(exportNames(target));
        },
      },
    ],
  });
  const { output } = await build.generate({ format: "esm" });
  await build.close?.();
  const dir = mkdtempSync(join(tmpdir(), "start-unit-"));
  const file = join(dir, "start.bundle.mjs");
  writeFileSync(file, output[0].code);
  try {
    return await import(pathToFileURL(file).href);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const A = await bundle();

// ---------------------------------------------------------------- 正常系（試験の土台）
console.log("正常系");
{
  reset();
  await A.beginMeeting(form(), null);
  const order = log.map((e) => e[0]);
  check("サーバに届けば会議 41 で録音を始める", captured().length === 1 && captured()[0][1] === 41 && captured()[0][3] === 0);
  check("マイク（ensureAudio）はサーバ呼び出しより前に開く", order.indexOf("ensureAudio") >= 0 && order.indexOf("ensureAudio") < order.indexOf("fetch") && calls.length === 3, order.join(","));
  check("ライブ画面へ移る", log.some((e) => e[0] === "navigate" && e[1] === "#/m/41"));
}

// ---------------------------------------------------------------- F1
console.log("F1: サーバが使えなければ端末だけで録音、4xx では切り替えない");
const offlineStarted = () => captured().length === 1 && captured()[0][1] < 0 && captured()[0][3] === 0;
const keptMic = () => count("ensureAudio") >= 1 && count("stopStream") === 1; // 入口の 1 回だけ（keepStream）
for (const [label, where, status] of [
  ["settings が 502", "GET /api/settings", 502],
  ["settings が 503", "GET /api/settings", 503],
  ["会議作成が 500", "POST /api/meetings", 500],
  ["start が 504", "POST /api/meetings/41/start", 504],
]) {
  reset();
  routes[where] = () => json({ error: "bad gateway" }, status);
  await A.beginMeeting(form(), null).catch(() => {});
  check(`${label} → オフラインの会議で録音を始める`, offlineStarted(), JSON.stringify(captured()));
  check(`${label} → マイクを開き直さない（プリロールを残す）`, keptMic(), `stopStream=${count("stopStream")}`);
}
{
  reset();
  routes["POST /api/meetings"] = () => {
    throw new TypeError("Failed to fetch");
  };
  await A.beginMeeting(form(), null).catch(() => {});
  check("会議作成が回線断（fetch が失敗） → オフラインの会議で録音を始める", offlineStarted());
}
{
  reset();
  routes["GET /api/settings"] = () => json({ error: "html from proxy" }, 502);
  await A.beginMeeting(form(), null).catch(() => {});
  check("オフラインに切り替えたとき、端末に録音していることを知らせる", log.some((e) => e[0] === "notify" && e[2] === "net"));
}
for (const [label, where, status] of [
  ["settings が 401", "GET /api/settings", 401],
  ["settings が 403", "GET /api/settings", 403],
  ["会議作成が 404", "POST /api/meetings", 404],
  ["start が 409", "POST /api/meetings/41/start", 409],
]) {
  reset();
  routes[where] = () => json({ error: "nope", code: "x" }, status);
  let error = null;
  await A.beginMeeting(form(), null).catch((err) => (error = err));
  check(`${label} → オフラインに切り替えず、エラーを返す`, error && error.status === status && captured().length === 0, String(error));
  check(`${label} → 開いたマイクを離す`, count("stopStream") === 2);
}

// ---------------------------------------------------------------- BUDGET
console.log("BUDGET: 開始系 API 全体で約 4 秒の締め切り");
{
  reset();
  routes["GET /api/settings"] = hang();
  const t0 = Date.now();
  await A.beginMeeting(form(), null).catch(() => {});
  const took = Date.now() - t0;
  check("settings が応答しない → 約 4 秒でオフライン録音に切り替わる", offlineStarted() && took >= 3800 && took <= 4800, `${took}ms`);
}
{
  // それぞれ 1.8 秒かかる回線。呼び出しごとに 4 秒なら 5.4 秒で成功してしまう（プリロール 6 秒の限界に近い）。
  reset();
  routes["GET /api/settings"] = slow(1800, () => json({ provider: "gemini", api_key_set: true }));
  routes["POST /api/meetings"] = slow(1800, () => json(detail(41)));
  routes["POST /api/meetings/41/start"] = slow(1800, () => json(detail(41)));
  const t0 = Date.now();
  await A.beginMeeting(form(), null).catch(() => {});
  const took = Date.now() - t0;
  const startCall = calls.find((c) => c.key === "POST /api/meetings/41/start");
  check("1 回 1.8 秒の遅い回線でも、合計の待ちは約 4〜4.6 秒で打ち切られる", took >= 3800 && took <= 4800, `${took}ms`);
  check("start には残り時間（最小 500ms）しか渡さない", !!startCall && Date.now() - startCall.at <= 800, startCall ? `${Date.now() - startCall.at}ms` : "start 未呼び出し");
  check("打ち切ったあとはオフラインの会議で録音を始める", offlineStarted());
}
{
  reset();
  routes["GET /api/settings"] = slow(1000, () => json({ provider: "gemini", api_key_set: true }));
  routes["POST /api/meetings"] = slow(1000, () => json(detail(41)));
  routes["POST /api/meetings/41/start"] = slow(1000, () => json(detail(41)));
  const t0 = Date.now();
  await A.beginMeeting(form(), null).catch(() => {});
  const took = Date.now() - t0;
  check("合計 3 秒で返る回線では、締め切りに掛からずサーバの会議で録音する", captured().length === 1 && captured()[0][1] === 41 && took < 3800, `${took}ms`);
}

// ---------------------------------------------------------------- F4
console.log("F4: 同時に押しても開始は 1 つ");
{
  reset();
  S.ensureAudioDelay = 150; // マイクの許可を待っている間に、もう 1 回押される
  const p1 = A.beginMeeting(form(), null);
  const p2 = A.beginMeeting(form(), null);
  check("2 回目の呼び出しは進行中の開始と同じ promise を返す", p1 === p2);
  await Promise.all([p1, p2]);
  check("マイクは 1 回だけ開く（ensureAudio 1 回・stopStream 1 回）", count("ensureAudio") === 1 && count("stopStream") === 1, `ensureAudio=${count("ensureAudio")} stopStream=${count("stopStream")}`);
  check("サーバの会議は 1 つだけ作る", calls.filter((c) => c.key === "POST /api/meetings").length === 1);
  check("録音の開始は 1 回だけ", captured().length === 1);
  reset();
  await A.beginMeeting(form(), null);
  check("終わったあとの開始は、ふつうに動く（ガードが外れている）", captured().length === 1 && count("ensureAudio") === 1);
}
{
  reset();
  routes["POST /api/meetings"] = () => json({ error: "nope" }, 404);
  const p1 = A.beginMeeting(form(), null);
  const p2 = A.beginMeeting(form(), null);
  const [r1, r2] = await Promise.allSettled([p1, p2]);
  check("失敗した開始でも、同時の 2 回目は同じ失敗を受け取る", r1.status === "rejected" && r2.status === "rejected" && p1 === p2);
  reset();
  await A.beginMeeting(form(), null);
  check("失敗のあとでもガードが外れ、次の開始が動く", captured().length === 1 && captured()[0][1] === 41);
}
{
  reset();
  S.ensureAudio = async () => {
    log.push(["ensureAudio"]);
    throw new Error("マイクの使用が許可されていません");
  };
  let error = null;
  await A.beginMeeting(form(), null).catch((err) => (error = err));
  check("マイクの許可が無ければ、サーバを呼ばずにエラーを返す", error && /許可/.test(error.message) && calls.length === 0 && captured().length === 0);
  S.ensureAudio = async () => {
    log.push(["ensureAudio"]);
  };
  reset();
  await A.beginMeeting(form(), null);
  check("マイクの失敗のあとでもガードが外れ、次の開始が動く", captured().length === 1);
}
{
  reset();
  await A.quickStart();
  check("quickStart はタブ音声を外し（micOnly）、Chrome の音声認識で開始する", count("micOnly") === 1 && captured().length === 1);
}

// ---------------------------------------------------------------- F3
console.log("F3: 端末の保管庫が応答しなくても録音は始まる");
{
  reset();
  S.vaultHang = true;
  routes["GET /api/settings"] = () => json({}, 503);
  const p = A.beginMeeting(form(), null);
  p.catch(() => {});
  await sleep(100);
  check("IndexedDB が応答しなくても startCapturing は呼ばれる", offlineStarted(), JSON.stringify(captured()));
  const state = await settleWithin(p, 1500);
  check(
    "IndexedDB が応答しなくても beginMeeting は（時間を区切って）終わる。ライブ画面へ移り、開始のガードも外れる",
    state !== "pending",
    "offline.ts は putMeeting / reload を最長 1.2 秒で見切る",
  );
  const again = A.beginMeeting(form(), null);
  again.catch(() => {});
  check("ガードが外れるので、そのあとの開始は前の promise を受け取らない", again !== p);
  S.vaultHang = false;
}

console.log(`\n${passes} passed, ${failures} failed${known.length ? `, ${known.length} known bug(s)` : ""}`);
process.exit(failures ? 1 : 0);
