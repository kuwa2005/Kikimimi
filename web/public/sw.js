// 画面のファイルを端末にキャッシュし、回線が無い・弱いときでも画面を開いて録音を始められるようにする。
// API（/api/）は扱わない（録音と書き起こしの保存は、画面側の保管庫 IndexedDB が受け持つ）。
const CACHE = "kikimimi-shell-v1";
const SHELL = ["/", "/capture-worklet.js"];
// 弱い回線で画面の読み込みを待ちすぎない。これを超えたらキャッシュの画面を出す。
const NAV_TIMEOUT_MS = 4000;

async function precache() {
  const cache = await caches.open(CACHE);
  await cache.addAll(SHELL);
  // 画面が読み込む資産（ハッシュ付きの JS・CSS）も先に入れておく。
  const html = await (await cache.match("/")).text();
  const assets = [...html.matchAll(/(?:src|href)="(\/assets\/[^"]+)"/g)].map((m) => m[1]);
  await cache.addAll(assets);
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
  try {
    const response = await Promise.race([fetch(request), timeout(NAV_TIMEOUT_MS)]);
    if (response.ok) {
      await cache.put("/", response.clone());
      // 新しい版の資産も取っておく（次にオフラインで開いたときのため）。
      precache().catch(() => {});
    }
    return response;
  } catch {
    const cached = await cache.match("/");
    if (cached) return cached;
    throw new Error("offline");
  }
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
