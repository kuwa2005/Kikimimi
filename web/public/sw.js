// 画面のファイルを端末にキャッシュし、回線が無い・弱いときでも画面を開いて録音を始められるようにする。
// API（/api/）は扱わない（録音と書き起こしの保存は、画面側の保管庫 IndexedDB が受け持つ）。
const CACHE = "kikimimi-shell-v1";
// 弱い回線で画面の読み込みを待ちすぎない。これを超えたらキャッシュの画面を出す。
const NAV_TIMEOUT_MS = 4000;

function assetsOf(html) {
  return [...html.matchAll(/(?:src|href)="(\/assets\/[^"]+)"/g)].map((m) => m[1]);
}

/**
 * 新しい画面を保存する。画面が読み込む資産（ハッシュ付きの JS・CSS）を先にすべて取り、
 * 取り終えてから "/" を差し替える。途中で回線が切れたら古い画面のまま残す（白い画面にしない）。
 */
async function refresh(response) {
  const html = await response.text();
  const assets = assetsOf(html);
  const cache = await caches.open(CACHE);
  for (const asset of assets) {
    if (await cache.match(asset)) continue;
    const got = await fetch(asset);
    if (!got.ok) throw new Error(`資産を取れません: ${asset}`);
    await cache.put(asset, got);
  }
  await cache.put("/", new Response(html, { headers: { "Content-Type": "text/html; charset=utf-8" } }));
  // 使わなくなった古い資産を消す。
  for (const request of await cache.keys()) {
    const path = new URL(request.url).pathname;
    if (path.startsWith("/assets/") && !assets.includes(path)) await cache.delete(request);
  }
}

async function precache() {
  const cache = await caches.open(CACHE);
  await cache.add("/capture-worklet.js");
  const response = await fetch("/", { cache: "no-store" });
  if (response.ok) await refresh(response);
}

self.addEventListener("install", (event) => {
  event.waitUntil(precache().then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      for (const key of await caches.keys()) if (key !== CACHE) await caches.delete(key);
      await self.clients.claim();
    })(),
  );
});

function timeout(ms) {
  return new Promise((_, reject) => setTimeout(() => reject(new Error("timeout")), ms));
}

async function navigation(request) {
  const cache = await caches.open(CACHE);
  let response;
  try {
    response = await Promise.race([fetch(request), timeout(NAV_TIMEOUT_MS)]);
  } catch {
    // 回線が無い・弱い。保存した画面を出す。
    return (await cache.match("/")) || Response.error();
  }
  if (!response.ok) {
    // サーバの更新中（502/503 など）。保存した画面で録音を続けられるようにする。
    return (await cache.match("/")) || response;
  }
  refresh(response.clone()).catch(() => {});
  return response;
}

async function asset(request) {
  const cache = await caches.open(CACHE);
  const cached = await cache.match(request);
  if (cached) return cached;
  const response = await fetch(request);
  if (response.ok) await cache.put(request, response.clone());
  return response;
}

async function other(request) {
  const cache = await caches.open(CACHE);
  try {
    const response = await fetch(request);
    if (response.ok) await cache.put(request, response.clone());
    return response;
  } catch {
    const cached = await cache.match(request);
    if (cached) return cached;
    throw new Error("offline");
  }
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin || url.pathname.startsWith("/api/")) return;
  if (request.mode === "navigate") event.respondWith(navigation(request));
  else if (url.pathname.startsWith("/assets/")) event.respondWith(asset(request));
  else event.respondWith(other(request));
});
