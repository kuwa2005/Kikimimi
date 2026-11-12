// 端末の保管庫に残る録音を、1 本の WAV に書き出す（最後の砦。サーバに送れないままでも手元に残せる）。
import * as vault from "./vault";

/** その会議の、端末に保存している録音区間と途中保存をつなげて WAV にし、ダウンロードさせる。区間の間の空きは無音で埋める。 */
export async function exportLocalAudio(meetingId: number, title: string) {
  const [chunks, tails] = await Promise.all([vault.listChunks(), vault.listTails()]);
  const pieces: { startMs: number; pcm: Int16Array }[] = [];
  for (const row of chunks) {
    if (row.meetingId === meetingId) pieces.push({ startMs: row.startMs, pcm: new Int16Array(row.wav.slice(44)) });
  }
  for (const tail of tails) {
    if (tail.meetingId === meetingId) pieces.push({ startMs: tail.startMs, pcm: new Int16Array(tail.pcm) });
  }
  if (!pieces.length) throw new Error("この端末には、書き出せる録音が残っていません（すでにサーバへ送り終えています）。");
  pieces.sort((a, b) => a.startMs - b.startMs);
  const origin = pieces[0].startMs * 16;
  let length = 0;
  for (const piece of pieces) length = Math.max(length, piece.startMs * 16 - origin + piece.pcm.length);
  const all = new Int16Array(length);
  for (const piece of pieces) all.set(piece.pcm, piece.startMs * 16 - origin);
  const blob = new Blob([vault.int16ToWav(all, 16000)], { type: "audio/wav" });
  const link = document.createElement("a");
  const url = URL.createObjectURL(blob);
  link.href = url;
  link.download = `${title.replace(/[\\/:*?"<>|]/g, "_")}.wav`;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 5000);
}
