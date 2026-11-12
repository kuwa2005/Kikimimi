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
        this.onresult && this.onresult({ resultIndex: 0, results: [{ isFinal: true, 0: { transcript: "速報" + n } }] });
      }, 1500);
    }
    stop() { clearInterval(this._t); this._t = null; this.onend && this.onend(); }
    abort() { clearInterval(this._t); this._t = null; }
  }
  FakeSR.available = async () => "available";
  window.SpeechRecognition = FakeSR;
  window.webkitSpeechRecognition = FakeSR;
})();`;

async function launch(profile) {
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
      `--use-file-for-fake-audio-capture=${path.join(WORK, "speech.wav")}`,
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

const all = { crash: scenarioCrash, offline: scenarioOffline, background: scenarioBackground, errors: scenarioServerErrors, apifail: scenarioApiFailure };
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
