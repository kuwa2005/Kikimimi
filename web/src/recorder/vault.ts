// 端末内の保管庫（IndexedDB）。録音と速報字幕を、サーバが受け取ったと答えるまで端末に残す。
// ブラウザが落ちる・タブを閉じる・PC が落ちる・オフラインになる、のどれでも、次に開いたときに続きを送る。
//
// - chunks:   区切り済みの録音区間（WAV）。送信が成功したら消す。
// - tail:     まだ区切っていない録音の途中（数秒ごとに上書き）。区切ったら消す。落ちたら次回に区間へ変える。
// - captions: Chrome の速報字幕（確定文）。送信が成功したら消す。
//
// IndexedDB が使えない環境（シークレットウィンドウの一部など）では、すべて失敗を返し、録音はメモリだけで続ける。

export type VaultChunk = {
  id?: number;
  userId: number;
  meetingId: number;
  startMs: number;
  wav: ArrayBuffer;
  silent: boolean;
  createdAt: number;
  /** 会議が録音を受け付けなくなったあとに残った区間。取り込み（X-Import）で送る。 */
  late?: boolean;
};

export type VaultTail = {
  key: string;
  userId: number;
  meetingId: number;
  startMs: number;
  /** 16kHz・16bit の PCM。 */
  pcm: ArrayBuffer;
  updatedAt: number;
};

export type VaultCaption = {
  id?: number;
  userId: number;
  meetingId: number;
  startSec: number;
  text: string;
  createdAt: number;
};

const NAME = "kikimimi-vault";
const VERSION = 1;

let opening: Promise<IDBDatabase> | null = null;
let broken = false;

function open(): Promise<IDBDatabase> {
  if (broken) return Promise.reject(new Error("端末内の保管庫が使えません"));
  if (opening) return opening;
  opening = new Promise<IDBDatabase>((resolve, reject) => {
    let req: IDBOpenDBRequest;
    try {
      req = indexedDB.open(NAME, VERSION);
    } catch (err) {
      reject(err);
      return;
    }
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains("chunks")) db.createObjectStore("chunks", { keyPath: "id", autoIncrement: true });
      if (!db.objectStoreNames.contains("tail")) db.createObjectStore("tail", { keyPath: "key" });
      if (!db.objectStoreNames.contains("captions")) db.createObjectStore("captions", { keyPath: "id", autoIncrement: true });
    };
    req.onsuccess = () => {
      const db = req.result;
      // 別のタブが版を上げるときは閉じて譲る（次の操作で開き直す）。
      db.onversionchange = () => {
        db.close();
        opening = null;
      };
      db.onclose = () => {
        opening = null;
      };
      resolve(db);
    };
    req.onerror = () => reject(req.error || new Error("保管庫を開けません"));
    req.onblocked = () => reject(new Error("保管庫を開けません（別のタブが使用中）"));
  }).catch((err) => {
    opening = null;
    if (typeof indexedDB === "undefined") broken = true;
    throw err;
  });
  return opening;
}

/** トランザクションが書き終わる（ディスクに確定する）まで待つ。 */
async function run<T>(stores: string[], mode: IDBTransactionMode, body: (tx: IDBTransaction) => T | Promise<T>): Promise<T> {
  const db = await open();
  // relaxed だと OS のキャッシュに載った時点で完了になる。録音は落ちても残したいので strict を求める。
  const tx = (db.transaction as (names: string[], mode: IDBTransactionMode, options?: { durability?: string }) => IDBTransaction)(stores, mode, {
    durability: "strict",
  });
  const done = new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error || new Error("保管庫への書き込みに失敗しました"));
    tx.onabort = () => reject(tx.error || new Error("保管庫への書き込みが中断されました"));
  });
  const result = await body(tx);
  await done;
  return result;
}

function reqValue<T>(req: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tailKey(userId: number, meetingId: number) {
  return `${userId}:${meetingId}`;
}

/** 区切った区間を保存し、同じ会議の途中保存（tail）を消す。両方を 1 つのトランザクションで行う。 */
export async function putChunk(chunk: VaultChunk): Promise<number> {
  return run(["chunks", "tail"], "readwrite", async (tx) => {
    const id = await reqValue(tx.objectStore("chunks").add(chunk));
    tx.objectStore("tail").delete(tailKey(chunk.userId, chunk.meetingId));
    return id as number;
  });
}

export async function markLate(id: number): Promise<void> {
  await run(["chunks"], "readwrite", async (tx) => {
    const store = tx.objectStore("chunks");
    const row = (await reqValue(store.get(id))) as VaultChunk | undefined;
    if (row && !row.late) store.put({ ...row, late: true });
  });
}

export async function deleteChunk(id: number): Promise<void> {
  await run(["chunks"], "readwrite", (tx) => {
    tx.objectStore("chunks").delete(id);
  });
}

export async function listChunks(): Promise<VaultChunk[]> {
  return run(["chunks"], "readonly", (tx) => reqValue(tx.objectStore("chunks").getAll()) as Promise<VaultChunk[]>);
}

export async function putTail(tail: Omit<VaultTail, "key">): Promise<void> {
  await run(["tail"], "readwrite", (tx) => {
    tx.objectStore("tail").put({ ...tail, key: tailKey(tail.userId, tail.meetingId) });
  });
}

export async function deleteTail(userId: number, meetingId: number): Promise<void> {
  await run(["tail"], "readwrite", (tx) => {
    tx.objectStore("tail").delete(tailKey(userId, meetingId));
  });
}

export async function listTails(): Promise<VaultTail[]> {
  return run(["tail"], "readonly", (tx) => reqValue(tx.objectStore("tail").getAll()) as Promise<VaultTail[]>);
}

export async function putCaption(caption: VaultCaption): Promise<number> {
  return run(["captions"], "readwrite", (tx) => reqValue(tx.objectStore("captions").add(caption)) as Promise<number>);
}

export async function deleteCaption(id: number): Promise<void> {
  await run(["captions"], "readwrite", (tx) => {
    tx.objectStore("captions").delete(id);
  });
}

export async function listCaptions(): Promise<VaultCaption[]> {
  return run(["captions"], "readonly", (tx) => reqValue(tx.objectStore("captions").getAll()) as Promise<VaultCaption[]>);
}

/** 会議を削除したとき、その会議の保管分を消す。 */
export async function forgetMeeting(meetingId: number): Promise<void> {
  await run(["chunks", "tail", "captions"], "readwrite", async (tx) => {
    for (const name of ["chunks", "tail", "captions"]) {
      const store = tx.objectStore(name);
      const rows = (await reqValue(store.getAll())) as { id?: number; key?: string; meetingId: number }[];
      for (const row of rows) if (row.meetingId === meetingId) store.delete((row.id ?? row.key) as IDBValidKey);
    }
  });
}

/** ログアウトで未送信分の破棄を選んだとき、その利用者の保管分を消す。 */
export async function forgetUser(userId: number): Promise<void> {
  await run(["chunks", "tail", "captions"], "readwrite", async (tx) => {
    for (const name of ["chunks", "tail", "captions"]) {
      const store = tx.objectStore(name);
      const rows = (await reqValue(store.getAll())) as { id?: number; key?: string; userId: number }[];
      for (const row of rows) if (row.userId === userId) store.delete((row.id ?? row.key) as IDBValidKey);
    }
  });
}

/** ブラウザに、保管庫を容量不足のときにも消さないよう頼む（Chrome は条件を満たせば許可する）。 */
export async function askPersistent(): Promise<boolean> {
  try {
    if (navigator.storage?.persisted && (await navigator.storage.persisted())) return true;
    return (await navigator.storage?.persist?.()) ?? false;
  } catch {
    return false;
  }
}

export function pcmToInt16(samples: Float32Array): Int16Array {
  const out = new Int16Array(samples.length);
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    out[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
  }
  return out;
}

/** 16bit PCM を WAV にする（tail を区間へ変えるとき）。 */
export function int16ToWav(pcm: Int16Array, rate: number): ArrayBuffer {
  const buffer = new ArrayBuffer(44 + pcm.length * 2);
  const view = new DataView(buffer);
  const write = (offset: number, text: string) => {
    for (let i = 0; i < text.length; i++) view.setUint8(offset + i, text.charCodeAt(i));
  };
  write(0, "RIFF");
  view.setUint32(4, 36 + pcm.length * 2, true);
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
  view.setUint32(40, pcm.length * 2, true);
  new Int16Array(buffer, 44).set(pcm);
  return buffer;
}

/** WAV の音声の長さ（16kHz・16bit・モノラル前提のサンプル数）。 */
export function wavSamples(wav: ArrayBuffer): number {
  return Math.max(0, (wav.byteLength - 44) / 2);
}
