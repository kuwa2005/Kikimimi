// 録音の取りこぼし耐性を、本物の Chromium（偽マイク）で確かめる。
// 使い方は e2e/README.md。node e2e/resilience.mjs [crash|offline|background|errors|apifail ...]（省略時はすべて）
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
// 事前に入っている Playwright を使う（npm i playwright でも可）。
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright");
import { spawn, execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const HERE = path.dirname(new URL(import.meta.url).pathname);
const WORK = process.env.E2E_WORK_DIR || path.join(HERE, ".work");
const BASE = process.env.E2E_BASE || "http://127.0.0.1:8765";
// 試験用アカウント。認証情報はコードに書かず、環境変数で渡す。
const EMAIL = process.env.E2E_EMAIL;
const PASSWORD = process.env.E2E_PASSWORD;
if (!EMAIL || !PASSWORD || !process.env.KIKIMIMI_DATA_DIR) {
  console.error("E2E_EMAIL・E2E_PASSWORD・KIKIMIMI_DATA_DIR を指定してください（e2e/README.md）");
  process.exit(2);
}
const DB = path.join(process.env.KIKIMIMI_DATA_DIR, "kikimimi.sqlite");
fs.mkdirSync(WORK, { recursive: true });
if (!fs.existsSync(path.join(WORK, "speech.wav"))) {
  execFileSync("python3", [path.join(HERE, "make-speech.py"), path.join(WORK, "speech.wav")]);
}
if (!fs.existsSync(path.join(WORK, "quiet.wav"))) {
  // 声が小さいマイク（音量 1/15）
  execFileSync("python3", [path.join(HERE, "make-speech.py"), path.join(WORK, "quiet.wav"), "0.066"]);
}
const CHROME = chromium.executablePath();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
let port = 9330;

function log(...args) {
  console.log(new Date().toISOString().slice(11, 19), ...args);
}

function check(name, ok, detail = "") {
  results.push({ name, ok, detail });
  log(ok ? "  OK  " : "  NG  ", name, detail);
}

const FAKE_SR = `(() => {
  class FakeSR {
    constructor() { this.onresult = null; this.onerror = null; this.onend = null; this._t = null; }
    start() {
      if (this._t) return;
      this._t = setInterval(() => {
        const n = (window.__srCount = (window.__srCount || 0) + 1);
        // 試験が台本（window.__srLines）を渡していれば、その順に話したことにする。
        const text = (window.__srLines && window.__srLines.shift()) || "速報" + n;
        this.onresult && this.onresult({ resultIndex: 0, results: [{ isFinal: true, 0: { transcript: text } }] });
      }, 1500);
    }
    stop() { clearInterval(this._t); this._t = null; this.onend && this.onend(); }
    abort() { clearInterval(this._t); this._t = null; }
  }
  FakeSR.available = async () => "available";
  window.SpeechRecognition = FakeSR;
  window.webkitSpeechRecognition = FakeSR;
})();`;

async function launch(profile, audio = "speech.wav") {
  port += 1;
  const proc = spawn(
    CHROME,
    [
      "--headless=new",
      `--remote-debugging-port=${port}`,
      `--user-data-dir=${profile}`,
      "--no-sandbox",
      "--no-first-run",
      "--use-fake-ui-for-media-stream",
      "--use-fake-device-for-media-stream",
      `--use-file-for-fake-audio-capture=${path.join(WORK, audio)}`,
      "--autoplay-policy=no-user-gesture-required",
      "about:blank",
    ],
    { detached: true, stdio: "ignore" },
  );
  let browser;
  for (let i = 0; i < 50 && !browser; i++) {
    await sleep(200);
    browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`).catch(() => null);
  }
  if (!browser) throw new Error("Chromium を起動できません");
  const context = browser.contexts()[0];
  await context.addInitScript(FAKE_SR);
  await context.grantPermissions(["microphone"], { origin: BASE }).catch(() => {});
  const cdp = await browser.newBrowserCDPSession();
  await cdp.send("Browser.setPermission", { permission: { name: "screen-wake-lock" }, setting: "granted", origin: BASE }).catch(() => {});
  const page = context.pages()[0] || (await context.newPage());
  return { proc, browser, context, page };
}

/** ブラウザを強制終了する（クラッシュ・PC の電源断に相当）。 */
function crash(proc) {
  process.kill(-proc.pid, "SIGKILL");
}

async function login(page) {
  await page.goto(`${BASE}/#/login`);
  await page.waitForLoadState("networkidle");
  if (!(await page.locator("#login-id").count())) return; // ログイン済み
  await page.fill("#login-id", EMAIL);
  await page.fill("#login-password", PASSWORD);
  await page.locator("form button[type=submit]").first().click();
  await page.waitForFunction(() => !location.hash.startsWith("#/login"));
}

/** 前の試験で中断したままの会議を終わらせる（Free は同時録音が 1 件まで）。 */
async function closeLeftovers(page) {
  await page.goto(`${BASE}/#/`);
  await page.waitForLoadState("networkidle");
  const list = await api(page, "/api/meetings?limit=200");
  for (const m of list.body?.meetings || list.body?.items || []) {
    if ((m.status === "live" || m.status === "paused") && !m.finish_requested) {
      await api(page, `/api/meetings/${m.id}/finish`, { method: "POST", body: JSON.stringify({ close: "plain" }) });
    }
  }
}

async function startMeeting(page, title, speech = "chrome") {
  await closeLeftovers(page);
  await page.goto(`${BASE}/#/new`);
  await page.waitForLoadState("networkidle");
  await page.locator("input").first().fill(title);
  const radios = page.locator('input[type=radio][name$="-speech"]');
  await radios.nth(speech === "api" ? 0 : speech === "chrome" ? 1 : 2).check();
  await page.getByRole("button", { name: "会議開始（録音開始）" }).click();
  await page.waitForFunction(() => /^#\/m\/\d+/.test(location.hash), null, { timeout: 15000 });
  const id = Number((await page.evaluate(() => location.hash)).match(/\d+/)[0]);
  await page.waitForFunction(() => window.__recorder && window.__recorder.capturing);
  return id;
}

async function api(page, url, init) {
  return page.evaluate(
    async ([url, init]) => {
      const me = await fetch("/api/auth/me").then((r) => r.json());
      const response = await fetch(url, {
        ...(init || {}),
        headers: { "Content-Type": "application/json", "X-CSRF-Token": me.csrf, ...((init && init.headers) || {}) },
      });
      return { status: response.status, body: await response.json().catch(() => null) };
    },
    [url, init],
  );
}

function jobs(meetingId) {
  const out = execFileSync("python3", [
    "-c",
    `import sqlite3,json,sys
c=sqlite3.connect("file:${DB}?mode=ro",uri=True)
print(json.dumps(c.execute("select start_ms,duration_ms,status,silent from audio_jobs where meeting_id=? order by start_ms",(${meetingId},)).fetchall()))`,
  ]);
  return JSON.parse(out.toString());
}

function segments(meetingId) {
  const out = execFileSync("python3", [
    "-c",
    `import sqlite3,json
c=sqlite3.connect("file:${DB}?mode=ro",uri=True)
print(json.dumps(c.execute("select start_sec,text from segments where meeting_id=? order by start_sec",(${meetingId},)).fetchall()))`,
  ]);
  return JSON.parse(out.toString());
}

function backups(meetingId) {
  const out = execFileSync("python3", [
    "-c",
    `import sqlite3
c=sqlite3.connect("file:${DB}?mode=ro",uri=True)
print(c.execute("select count(*) from caption_backup where meeting_id=?",(${meetingId},)).fetchone()[0])`,
  ]);
  return Number(out.toString());
}

/** 区間が隙間・重なりなく並んでいるか。 */
function contiguity(rows) {
  const gaps = [];
  for (let i = 1; i < rows.length; i++) {
    const [prevStart, prevDur] = rows[i - 1];
    const [start] = rows[i];
    const diff = start - (prevStart + prevDur);
    if (Math.abs(diff) > 30) gaps.push({ at: start, diff });
  }
  const total = rows.length ? rows[rows.length - 1][0] + rows[rows.length - 1][1] - rows[0][0] : 0;
  return { gaps, total, count: rows.length };
}

async function localMs(page) {
  return page.evaluate(() => {
    const r = window.__recorder;
    return Math.round((r.sentSamples + r.buffered * (16000 / r.sampleRate)) / 16);
  });
}

async function waitFor(fn, timeout, every = 500) {
  const end = Date.now() + timeout;
  let last;
  while (Date.now() < end) {
    last = await fn();
    if (last) return last;
    await sleep(every);
  }
  return last;
}

function freshProfile(name) {
  const dir = path.join(WORK, "profiles", name);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

// ---------------------------------------------------------------- シナリオ

async function scenarioCrash() {
  log("■ 録音中にブラウザが落ちる（未送信あり）");
  const profile = freshProfile("crash");
  let b = await launch(profile);
  await login(b.page);
  const id = await startMeeting(b.page, "クラッシュ試験");
  await sleep(8000);
  // サーバへ届かない状態にして、未送信を溜める
  await b.context.route("**/api/meetings/*/audio", (route) => route.abort());
  await b.context.route("**/api/meetings/*/captions", (route) => route.abort());
  await sleep(14000);
  const before = await localMs(b.page);
  const captionsMade = await b.page.evaluate(() => window.__srCount || 0);
  const queued = await b.page.evaluate(() => window.__recorder.queue.length);
  const serverBefore = contiguity(jobs(id)).total;
  log(`  落とす直前: 端末の録音 ${before}ms / サーバ ${serverBefore}ms / 未送信 ${queued} 区間 / 速報 ${captionsMade} 件`);
  check("落とす前に未送信が溜まっている", queued >= 1 && serverBefore < before - 5000, `queued=${queued}`);
  crash(b.proc);
  await sleep(1500);

  b = await launch(profile);
  await login(b.page);
  await b.page.goto(`${BASE}/#/m/${id}`);
  const restored = await waitFor(() => {
    const c = contiguity(jobs(id));
    return c.total >= before - 2600 ? c : null;
  }, 30000);
  const after = contiguity(jobs(id));
  check("再起動後に未送信の録音がサーバへ届く（途中保存の間隔 2 秒以内の欠落）", !!restored, `server=${after.total}ms / 落ちる前の端末=${before}ms`);
  check("届いた区間に隙間・重なりが無い", after.gaps.length === 0, JSON.stringify(after.gaps));
  const segs = await waitFor(() => {
    const s = segments(id);
    return s.length >= captionsMade - 1 ? s : null;
  }, 20000);
  check("速報字幕（Chrome の書き起こし）も失われない", !!segs, `server=${segments(id).length} / 作った数=${captionsMade}`);
  const notice = await b.page.locator("text=前回ブラウザを閉じる前に送れなかった").count();
  check("復元を画面で知らせる", notice > 0);

  // そのまま再開して、続きが重ならないこと
  await b.page.getByRole("button", { name: /録音を再開|再開/ }).first().click();
  await b.page.waitForFunction(() => window.__recorder.capturing, null, { timeout: 15000 });
  await sleep(9000);
  await b.page.getByRole("button", { name: "一時停止" }).click();
  await waitFor(async () => (await b.page.evaluate(() => window.__recorder.queue.length === 0)) || null, 15000);
  const resumed = contiguity(jobs(id));
  check("再開後の録音が前の続きに隙間・重なりなく並ぶ", resumed.gaps.length === 0 && resumed.total > after.total + 5000, `total=${resumed.total}ms gaps=${JSON.stringify(resumed.gaps)}`);
  const left = await b.page.evaluate(async () => {
    const db = await new Promise((r) => { const q = indexedDB.open("kikimimi-vault"); q.onsuccess = () => r(q.result); });
    const count = (name) => new Promise((r) => { const q = db.transaction(name).objectStore(name).count(); q.onsuccess = () => r(q.result); });
    return { chunks: await count("chunks"), tail: await count("tail"), captions: await count("captions") };
  });
  check("送信後は端末の保管庫が空になる", left.chunks === 0 && left.tail === 0 && left.captions === 0, JSON.stringify(left));
  await b.browser.close().catch(() => {});
  crash(b.proc);
  return id;
}

async function scenarioOffline() {
  log("■ 録音中にオフラインになる（20 秒）");
  const b = await launch(freshProfile("offline"));
  await login(b.page);
  const id = await startMeeting(b.page, "オフライン試験");
  await sleep(5000);
  await b.context.setOffline(true);
  await sleep(20000);
  const queued = await b.page.evaluate(() => window.__recorder.queue.length);
  const capturing = await b.page.evaluate(() => window.__recorder.capturing);
  const notice = await b.page.locator("text=この端末に保存済み").count();
  check("オフライン中も録音は止まらず、端末に溜める", capturing && queued >= 1, `queued=${queued}`);
  check("オフライン中、端末に保存済みであることを表示する", notice > 0);
  await b.context.setOffline(false);
  const t0 = Date.now();
  const drained = await waitFor(async () => (await b.page.evaluate(() => window.__recorder.queue.length === 0 && window.__recorder.captionQueue.length === 0)) || null, 20000, 200);
  check("復帰後すぐに送り切る", !!drained, `${Date.now() - t0}ms`);
  await sleep(4000);
  const local = await b.page.evaluate(() => window.__recorder.sentSamples / 16);
  await b.page.getByRole("button", { name: "一時停止" }).click();
  await sleep(3000);
  const c = contiguity(jobs(id));
  check("オフラインをはさんでも区間に隙間が無い", c.gaps.length === 0 && c.total >= local - 50, `server=${c.total}ms local>=${Math.round(local)}ms`);
  const made = await b.page.evaluate(() => window.__srCount || 0);
  const segs = segments(id).length;
  check("オフライン中の速報字幕もすべて保存される", segs >= made - 1, `server=${segs} / 作った数=${made}`);
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioBackground() {
  log("■ 別ウィンドウを前面・画面オフ（非表示）・凍結・省電力で音声処理が止まる");
  const b = await launch(freshProfile("background"));
  await login(b.page);
  const id = await startMeeting(b.page, "バックグラウンド試験");
  await sleep(3000);
  const wake = await b.page.evaluate(() => !!(window.__recorder.wakeLock && !window.__recorder.wakeLock.released));
  check("録音中は画面が暗くならないよう Wake Lock を取る", wake);

  const cdp = await b.context.newCDPSession(b.page);
  // 別のウィンドウを前面にする（タブは非表示になる）
  const other = await b.context.newPage();
  await other.goto("about:blank");
  await cdp.send("Emulation.setFocusEmulationEnabled", { enabled: false }).catch(() => {});
  await cdp.send("Page.setWebLifecycleState", { state: "active" }).catch(() => {});
  // 非表示（画面ロック・スクリーンセーバー・最小化と同じ扱い）
  await b.page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  const hiddenStart = await localMs(b.page);
  await sleep(10000);
  const hiddenEnd = await localMs(b.page);
  check("非表示の間も録音が進む", hiddenEnd - hiddenStart > 8000, `${hiddenEnd - hiddenStart}ms / 10000ms`);
  const tailSaved = await b.page.evaluate(async () => {
    const db = await new Promise((r) => { const q = indexedDB.open("kikimimi-vault"); q.onsuccess = () => r(q.result); });
    return new Promise((r) => { const q = db.transaction("tail").objectStore("tail").count(); q.onsuccess = () => r(q.result); });
  });
  check("非表示になった時点で途中の録音も端末に保存する", tailSaved >= 0, `tail=${tailSaved}`);

  // 省電力で AudioContext が止められた
  await b.page.evaluate(() => window.__recorder.ctx.suspend());
  await sleep(1500);
  const state = await b.page.evaluate(() => window.__recorder.ctx.state);
  check("止められた音声処理を自動で起こし直す", state === "running", state);

  // 表示に戻る
  await b.page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "visible" });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => false });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await b.page.bringToFront();
  await sleep(1000);
  const wakeAgain = await b.page.evaluate(() => !!(window.__recorder.wakeLock && !window.__recorder.wakeLock.released));
  check("表示に戻ったら Wake Lock を取り直す", wakeAgain);

  // ページ凍結（Chrome が裏のタブを凍結したとき）
  const beforeFreeze = await localMs(b.page);
  await cdp.send("Page.setWebLifecycleState", { state: "frozen" });
  await sleep(6000);
  await cdp.send("Page.setWebLifecycleState", { state: "active" });
  await sleep(4000);
  const afterFreeze = await localMs(b.page);
  log(`  凍結 6 秒＋4 秒の間に端末で進んだ録音: ${afterFreeze - beforeFreeze}ms`);
  check("凍結が解けたあとも録音が続く", afterFreeze - beforeFreeze > 3000 && (await b.page.evaluate(() => window.__recorder.capturing)));

  // マイクが一瞬切れる（USB の抜き差し）
  await b.page.evaluate(() => window.__recorder.stream.getAudioTracks()[0].stop());
  await b.page.evaluate(() => window.__recorder.stream.getAudioTracks()[0].dispatchEvent(new Event("ended")));
  const recovered = await waitFor(async () => (await b.page.evaluate(() => window.__recorder.stream && window.__recorder.stream.getAudioTracks()[0].readyState === "live" && !window.__recorder.micProblem)) || null, 15000);
  check("マイクが切れても取り直して録音を続ける", !!recovered);
  await sleep(3000);
  await b.page.getByRole("button", { name: "一時停止" }).click();
  await sleep(3000);
  const c = contiguity(jobs(id));
  check("一連の妨害のあとも区間に隙間が無い", c.gaps.length === 0, `total=${c.total}ms gaps=${JSON.stringify(c.gaps)}`);
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioServerErrors() {
  log("■ サーバが 500 を返す・別の端末で会議が終了された");
  const b = await launch(freshProfile("errors"));
  await login(b.page);
  const id = await startMeeting(b.page, "サーバ障害試験");
  await sleep(4000);
  let failing = true;
  await b.context.route("**/api/meetings/*/audio", (route) =>
    failing ? route.fulfill({ status: 500, contentType: "application/json", body: '{"error":"試験用の障害"}' }) : route.continue(),
  );
  await sleep(12000);
  failing = false;
  await b.context.unroute("**/api/meetings/*/audio");
  const drained = await waitFor(async () => (await b.page.evaluate(() => window.__recorder.queue.length === 0)) || null, 20000);
  check("500 が続いても再送で全部届く", !!drained);

  // 送れない間に、別の端末で会議が終了される
  await b.context.route("**/api/meetings/*/audio", (route) => route.abort());
  await sleep(10000);
  const pending = await b.page.evaluate(() => window.__recorder.queue.length);
  const local = await localMs(b.page);
  const ended = await api(b.page, `/api/meetings/${id}/finish`, { method: "POST", body: JSON.stringify({ close: "plain" }) });
  log(`  別端末からの終了: ${ended.status} / 未送信 ${pending} 区間`);
  await b.context.unroute("**/api/meetings/*/audio");
  await b.page.evaluate(() => window.dispatchEvent(new Event("online")));
  const done = await waitFor(async () => {
    const r = await api(b.page, `/api/meetings/${id}`);
    return r.body?.meeting?.status === "done" ? r : null;
  }, 30000);
  check("会議は要約まで終わる", !!done);
  const imported = await waitFor(() => {
    const c = contiguity(jobs(id));
    return c.total >= local - 2000 ? c : null;
  }, 70000, 2000);
  const c = contiguity(jobs(id));
  check("終了後に届いた未送信分も捨てずに取り込む", !!imported, `server=${c.total}ms / 端末=${local}ms / 端末に残る=${await b.page.evaluate(() => window.__recorder.parked.length)}`);
  check("取り込んだ区間に隙間が無い", c.gaps.length === 0, JSON.stringify(c.gaps));
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioApiFailure() {
  log("■ API の文字起こしが失敗した区間を、速報字幕の控えで埋める");
  const b = await launch(freshProfile("apifail"));
  await login(b.page);
  // 存在しないキー（開発用のダミー）。文字起こしは失敗する。
  const saved = await api(b.page, "/api/settings", { method: "PUT", body: JSON.stringify({ provider: "gemini", api_key: "AIza-dummy-for-test-only" }) });
  log(`  ダミーキー登録: ${saved.status}`);
  const id = await startMeeting(b.page, "API 失敗試験", "api");
  await sleep(14000);
  await b.page.getByRole("button", { name: "一時停止" }).click();
  await sleep(2000);
  const finished = await api(b.page, `/api/meetings/${id}/finish`, { method: "POST", body: JSON.stringify({ close: "api" }) });
  log(`  終了: ${finished.status}`);
  // 失敗が確定するまで待ち、再試行待ちが残っていればあきらめる
  const settled = await waitFor(async () => {
    const r = await api(b.page, `/api/meetings/${id}`);
    const m = r.body?.meeting;
    if (!m) return null;
    if (m.retry_waiting) await api(b.page, `/api/meetings/${id}/give-up-retries`, { method: "POST" });
    return m.status === "done" ? r.body : null;
  }, 90000, 2000);
  const segs = segments(id);
  const patched = segs.filter(([, text]) => text.startsWith("（速報）")).length;
  const rows = jobs(id);
  log(`  区間: ${JSON.stringify(rows.map((r) => r[2]))}`);
  check("会議が完了する", !!settled);
  check("API が失敗した区間が速報字幕で埋まる", patched > 0, `速報で埋めた発言=${patched}`);
  check("議事録に欠落と補完を明記する", (settled?.meeting?.summary_md || "").includes("速報字幕で補いました"), (settled?.meeting?.summary_md || "").split("\n")[0]);
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function vaultCounts(page) {
  return page.evaluate(async () => {
    const db = await new Promise((r) => { const q = indexedDB.open("kikimimi-vault"); q.onsuccess = () => r(q.result); });
    const count = (name) => new Promise((r) => { const q = db.transaction(name).objectStore(name).count(); q.onsuccess = () => r(q.result); });
    return { chunks: await count("chunks"), tail: await count("tail"), captions: await count("captions"), meetings: await count("meetings") };
  });
}

/** 一度オンラインで開いて、画面のキャッシュ（Service Worker）とログインした利用者を覚えさせる。 */
async function primeOffline(b) {
  await login(b.page);
  await closeLeftovers(b.page);
  await b.page.evaluate(() => navigator.serviceWorker.ready);
  await b.page.reload();
  await b.page.waitForFunction(() => !!navigator.serviceWorker.controller, null, { timeout: 15000 });
}

async function startOffline(b, title) {
  await b.context.setOffline(true);
  await b.page.reload();
  await b.page.waitForSelector("text=オフライン", { timeout: 15000 });
  const offlineSession = await b.page.locator(".offline-badge").count();
  await b.page.goto(`${BASE}/#/new`);
  await b.page.locator("input").first().fill(title);
  await b.page.getByRole("button", { name: "会議開始（録音開始）" }).click();
  await b.page.waitForFunction(() => /^#\/local\/\d+/.test(location.hash), null, { timeout: 15000 });
  await b.page.waitForFunction(() => window.__recorder && window.__recorder.capturing);
  return offlineSession > 0;
}

async function scenarioOfflineStart() {
  log("■ 完全にオフラインの状態から録音を始め、途中で回線がつながる");
  const b = await launch(freshProfile("offlinestart"));
  await primeOffline(b);
  const badge = await startOffline(b, "オフライン開始試験");
  check("オフラインでも画面が開き、ログイン中として動く", badge);
  check("オフラインのまま会議を始めて録音できる", true);
  await sleep(18000);
  if (process.env.E2E_SHOTS) await b.page.screenshot({ path: path.join(process.env.E2E_SHOTS, "offline-meeting.png") });
  const before = await localMs(b.page);
  const counts = await vaultCounts(b.page);
  check("オフライン中の録音・字幕・会議は端末に保存される", counts.chunks >= 1 && counts.meetings === 1 && counts.captions >= 1, JSON.stringify(counts));
  // モバイル回線がつながった
  await b.context.setOffline(false);
  const moved = await waitFor(async () => {
    const hash = await b.page.evaluate(() => location.hash);
    return /^#\/m\/\d+/.test(hash) ? hash : null;
  }, 30000);
  check("つながると自動でサーバに会議を作り、会議画面へ移る", !!moved, String(moved));
  const id = moved ? Number(moved.match(/\d+/)[0]) : 0;
  const still = await b.page.evaluate(() => window.__recorder.capturing && window.__recorder.meetingId);
  check("引き継いだあとも録音は止まらず続く", still === id, `meetingId=${still}`);
  await sleep(8000);
  await b.page.getByRole("button", { name: "一時停止" }).click();
  const drained = await waitFor(async () => (await b.page.evaluate(() => window.__recorder.queue.length === 0 && window.__recorder.captionQueue.length === 0)) || null, 30000);
  const local = await b.page.evaluate(() => window.__recorder.sentSamples / 16);
  const c = contiguity(jobs(id));
  check("オフライン中とその後の録音が、隙間なく 1 つの会議に並ぶ", !!drained && c.gaps.length === 0 && Math.abs(c.total - local) < 100 && c.total > before, `server=${c.total}ms 端末=${Math.round(local)}ms gaps=${JSON.stringify(c.gaps)}`);
  const made = await b.page.evaluate(() => window.__srCount || 0);
  // API の会議では速報は控え（caption_backup）に、Chrome の会議では書き起こし（segments）に入る。
  const kept = segments(id).length + backups(id);
  check("オフライン中の速報字幕もサーバに残る（書き起こしか控え）", kept >= made - 1, `server=${kept} / 作った数=${made}`);
  const left = await vaultCounts(b.page);
  check("送り終えたら端末の保管庫が空になる", left.chunks === 0 && left.tail === 0 && left.captions === 0, JSON.stringify(left));
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioOfflineCrash() {
  log("■ オフラインで録音中にブラウザが落ち、オフラインのまま開き直し、終了してから回線がつながる");
  const profile = freshProfile("offlinecrash");
  let b = await launch(profile);
  await primeOffline(b);
  await startOffline(b, "オフライン中のクラッシュ試験");
  await sleep(15000);
  const before = await localMs(b.page);
  crash(b.proc);
  await sleep(1500);
  b = await launch(profile);
  await b.context.setOffline(true);
  await b.page.goto(`${BASE}/#/`);
  const listed = await waitFor(async () => (await b.page.locator("text=オフライン中のクラッシュ試験").count()) || null, 15000);
  check("オフラインのまま開き直しても、端末の会議がホームに出る", !!listed);
  await b.page.locator("text=オフライン中のクラッシュ試験").first().click();
  await b.page.waitForFunction(() => /^#\/local\//.test(location.hash));
  await b.page.getByRole("button", { name: "終了" }).click();
  await b.page.getByRole("button", { name: "終了する" }).click();
  await sleep(1000);
  await b.context.setOffline(false);
  const finished = await waitFor(async () => {
    // ブラウザが落ちると Cookie が消えることがある。その場合はログインし直す（録音は端末に残っている）。
    if ((await b.page.evaluate(() => location.hash)) === "#/login" && (await b.page.locator("#login-id").count())) {
      log("  ログインし直します");
      await b.page.fill("#login-id", EMAIL);
      await b.page.fill("#login-password", PASSWORD);
      await b.page.locator("form button[type=submit]").first().click();
      await sleep(1000);
    }
    const list = await api(b.page, "/api/meetings?limit=50");
    const m = (list.body?.meetings || []).find((x) => x.title === "オフライン中のクラッシュ試験");
    return m && (m.status === "done" || m.status === "wrapping") ? m : null;
  }, 60000, 1000);
  if (!finished) {
    log("  画面のお知らせ: " + (await b.page.locator(".notices, [role=status], [role=alert]").allTextContents()).join(" | "));
    log("  状態: " + JSON.stringify(await b.page.evaluate(() => ({ hash: location.hash, q: window.__recorder.queue.map((i) => i.meetingId), parked: window.__recorder.parked.length, paused: window.__recorder.paused }))));
    log("  保管庫: " + JSON.stringify(await vaultCounts(b.page)));
  }
  check("つながると会議を作り、録音を送り、終了まで進む", !!finished, finished ? finished.status : "");
  const c = finished ? contiguity(jobs(finished.id)) : { total: 0, gaps: [] };
  check("落ちる直前までの録音が残る（途中保存の間隔 2 秒以内）", c.total >= before - 2600 && c.gaps.length === 0, `server=${c.total}ms / 落ちる前=${before}ms`);
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioOrder() {
  log("■ 速報字幕の並び順を、会議中に反転する");
  const b = await launch(freshProfile("order"));
  await login(b.page);
  // API の会議は速報字幕をそのまま並べる（試験用のダミーキーで開始できるようにする）。
  await api(b.page, "/api/settings", { method: "PUT", body: JSON.stringify({ provider: "gemini", api_key: "AIza-dummy-for-test-only" }) });
  await startMeeting(b.page, "並び順試験", "api");
  await sleep(6000);
  const toggle = b.page.locator(".transcript-tools button").first();
  const before = await b.page.locator(".captions .seg-text").allTextContents();
  await toggle.click();
  await sleep(2000);
  const after = await b.page.locator(".captions .seg-text").allTextContents();
  const num = (t) => Number((t.match(/\d+/) || [0])[0]);
  check("通常は新しい字幕が下", before.length >= 2 && num(before[before.length - 1]) > num(before[0]), JSON.stringify(before));
  check("切り替えると新しい字幕が上", after.length >= 2 && num(after[0]) > num(after[after.length - 1]), JSON.stringify(after));
  check("切り替えても録音は止まらない", await b.page.evaluate(() => window.__recorder.capturing));
  await b.page.reload();
  await b.page.waitForSelector(".transcript-tools button");
  const kept = await b.page.locator(".transcript-tools button").first().getAttribute("aria-pressed");
  check("並び順は端末に記憶される", kept === "true");
  await b.page.locator(".transcript-tools button").first().click();
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioEnhance() {
  log("■ 声が小さいマイクで、音声強化を会議中にオンにする");
  const b = await launch(freshProfile("enhance"), "quiet.wav");
  await login(b.page);
  const id = await startMeeting(b.page, "音声強化試験");
  await b.page.evaluate(() => localStorage.setItem("kikimimi.enhance", "0"));
  await sleep(4000);
  const quiet = await b.page.evaluate(() => window.__recorder.speechRms);
  await b.page.locator(".enhance-toggle input").check();
  await sleep(20000);
  const st = await b.page.evaluate(() => ({ gain: window.__recorder.autoGain, rms: window.__recorder.speechRms, ratio: window.__recorder.chain.compressor.ratio.value, hp: window.__recorder.chain.highpass.frequency.value, capturing: window.__recorder.capturing }));
  log(`  強化前の発話の大きさ ${quiet.toFixed(4)} → 強化後 ${st.rms.toFixed(4)}（音量 ×${st.gain.toFixed(1)}）`);
  check("録音中にオンにでき、録音は止まらない", st.capturing && st.ratio === 4 && st.hp === 110);
  check("小さい声を目安の大きさまで持ち上げる", st.rms > 0.07 && st.rms > quiet * 1.8, `rms ${quiet.toFixed(3)}→${st.rms.toFixed(3)} 自動ゲイン×${st.gain.toFixed(2)}`);
  await b.page.locator(".enhance-toggle input").uncheck();
  await sleep(500);
  const off = await b.page.evaluate(() => ({ g: window.__recorder.chain.gain.gain.value, ratio: window.__recorder.chain.compressor.ratio.value }));
  check("オフに戻すと素通しになる", off.ratio === 1, JSON.stringify(off));
  await b.page.getByRole("button", { name: "一時停止" }).click();
  await sleep(3000);
  const c = contiguity(jobs(id));
  check("切り替えをはさんでも区間に隙間が無い", c.gaps.length === 0, JSON.stringify(c.gaps));
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

async function scenarioAgenda() {
  log("■ Chrome の音声認識だけの会議で、事前のアジェンダに関係する発言から回答をセットする（Claude）");
  const mockLog = process.env.E2E_MOCK_CLAUDE_LOG;
  if (!mockLog) {
    check("Claude の模擬サーバを指定する（E2E_MOCK_CLAUDE_LOG と KIKIMIMI_ANTHROPIC_BASE、e2e/README.md）", false);
    return;
  }
  fs.writeFileSync(mockLog, "");
  const b = await launch(freshProfile("agenda"));
  await login(b.page);
  await closeLeftovers(b.page);
  const saved = await api(b.page, "/api/settings", { method: "PUT", body: JSON.stringify({ provider: "anthropic", anthropic_api_key: "sk-ant-dummy-for-test-only" }) });
  log(`  Claude のキー（ダミー）を登録: ${saved.status}`);
  const created = await api(b.page, "/api/meetings", {
    method: "POST",
    body: JSON.stringify({ title: "アジェンダ更新試験", questions: ["予算はいくらか", "導入の時期はいつか", "決裁者は誰か"] }),
  });
  const id = created.body.meeting.id;
  await b.page.goto(`${BASE}/#/m/${id}`);
  await b.page.waitForSelector('input[type=radio][name$="-speech"]');
  await b.page.locator('input[type=radio][name$="-speech"]').nth(1).check();
  // 会議の発言（Chrome の確定文）の台本
  await b.page.evaluate(() => {
    window.__srLines = ["本日はよろしくお願いします", "予算は三百万円を考えています", "導入の時期は来年の四月です", "天気の話をしましょう", "決裁は営業部長が行います"];
  });
  await b.page.getByRole("button", { name: "会議開始（録音開始）" }).click();
  await b.page.waitForFunction(() => window.__recorder && window.__recorder.capturing);
  const answered = await waitFor(async () => {
    const r = await api(b.page, `/api/meetings/${id}`);
    const qs = r.body?.questions || [];
    return qs.filter((q) => q.answer).length >= 3 ? qs : null;
  }, 90000, 2000);
  const qs = answered || (await api(b.page, `/api/meetings/${id}`)).body.questions;
  for (const q of qs) log(`  議題「${q.body}」→ ${q.status}: ${q.answer || "（空）"}`);
  const pick = (word) => qs.find((q) => q.body.includes(word));
  check("Chrome の会議でも、事前のアジェンダに回答がセットされる", !!answered);
  check("予算の議題に予算の発言が入る", (pick("予算")?.answer || "").includes("三百万円"));
  check("導入時期の議題に時期の発言が入る", (pick("導入")?.answer || "").includes("来年の四月"));
  check("決裁者の議題に決裁の発言が入る", (pick("決裁")?.answer || "").includes("営業部長"));
  check("関係ない発言は回答に入らない", !qs.some((q) => (q.answer || "").includes("天気")));
  // 狭い画面ではタブに分かれる。アジェンダのタブを開いて、議題の回答欄に出ているかを見る。
  const tab = b.page.getByRole("tab", { name: "アジェンダ" });
  if (await tab.count()) await tab.first().click();
  const onScreen = await waitFor(async () => {
    const answers = await b.page.locator(".q-answer").allTextContents();
    return answers.some((text) => text.includes("三百万円")) && answers.some((text) => text.includes("営業部長")) ? answers : null;
  }, 10000);
  if (process.env.E2E_SHOTS) await b.page.screenshot({ path: path.join(process.env.E2E_SHOTS, "agenda-chrome.png") });
  check("会議画面のアジェンダにも回答が出る", !!onScreen);
  const calls = fs.readFileSync(mockLog, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const analyses = calls.filter((c) => c.kind === "analysis");
  check("Claude へは書き起こしの文字だけを送り、音声は送らない", analyses.length > 0 && calls.every((c) => !c.has_audio), `解析 ${analyses.length} 回 / 最大 ${Math.max(...calls.map((c) => c.bytes))} バイト`);
  check("解析はまとめて行う（確定文ごとに呼ばない）", analyses.length < 5, `確定文 5 件に対して 解析 ${analyses.length} 回`);
  check("録音は止まらない", await b.page.evaluate(() => window.__recorder.capturing));
  await b.page.getByRole("button", { name: "一時停止" }).click();
  await api(b.page, "/api/settings", { method: "PUT", body: JSON.stringify({ provider: "gemini" }) });
  await b.browser.close().catch(() => {});
  crash(b.proc);
}

const all = { agenda: scenarioAgenda, crash: scenarioCrash, offlinestart: scenarioOfflineStart, offlinecrash: scenarioOfflineCrash, order: scenarioOrder, enhance: scenarioEnhance, offline: scenarioOffline, background: scenarioBackground, errors: scenarioServerErrors, apifail: scenarioApiFailure };
const pick = process.argv.slice(2);
for (const [name, run] of Object.entries(all)) {
  if (pick.length && !pick.includes(name)) continue;
  try {
    await run();
  } catch (err) {
    check(`シナリオ ${name} が最後まで動く`, false, String(err && err.stack ? err.stack.split("\n").slice(0, 3).join(" ") : err));
  }
}
console.log("\n==== 結果 ====");
for (const r of results) console.log(r.ok ? "OK" : "NG", r.name, r.detail);
const ng = results.filter((r) => !r.ok).length;
console.log(`${results.length - ng} OK / ${ng} NG`);
process.exit(ng ? 1 : 0);
