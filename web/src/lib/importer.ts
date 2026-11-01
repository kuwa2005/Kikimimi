// ボイスレコーダーの音声ファイルと、文字起こし済みテキストの取り込み（#96）。
// 音声はブラウザで 16kHz・モノラルにし、会議中の録音と同じ区間（最長 15 秒）に分けて送る。
import { request } from "../api/client";
import { encodeWav } from "../recorder/recorder";

const RATE = 16000;

/** Chrome が読める音声ファイルを 16kHz・モノラルにする。 */
export async function decodeAudioFile(file: File): Promise<Float32Array> {
  const data = await file.arrayBuffer();
  const probe = new AudioContext();
  let decoded: AudioBuffer;
  try {
    decoded = await probe.decodeAudioData(data);
  } catch {
    throw new Error("この音声ファイルを読めません。mp3・m4a・wav・ogg などの形式で保存し直してください");
  } finally {
    void probe.close();
  }
  const length = Math.ceil(decoded.duration * RATE);
  if (!length) throw new Error("音声が空です");
  const offline = new OfflineAudioContext(1, length, RATE);
  const source = offline.createBufferSource();
  source.buffer = decoded;
  source.connect(offline.destination);
  source.start();
  const rendered = await offline.startRendering();
  return rendered.getChannelData(0).slice();
}

function rms(samples: Float32Array, from: number, to: number): number {
  let sum = 0;
  const end = Math.min(to, samples.length);
  for (let i = from; i < end; i++) sum += samples[i] * samples[i];
  return Math.sqrt(sum / Math.max(1, end - from));
}

export type Piece = { startSec: number; samples: Float32Array; silent: boolean };

/** 10〜15 秒の間でいちばん静かな所で区切る（言葉の途中で切らないため）。 */
export function splitPieces(samples: Float32Array): Piece[] {
  const pieces: Piece[] = [];
  const step = RATE / 10; // 100ms
  let start = 0;
  while (start < samples.length) {
    let end = Math.min(samples.length, start + 15 * RATE);
    if (end < samples.length) {
      let best = end;
      let quietest = Infinity;
      for (let at = start + 10 * RATE; at + step <= end; at += step) {
        const level = rms(samples, at, at + step);
        if (level < quietest) {
          quietest = level;
          best = at + step / 2;
        }
      }
      end = Math.round(best);
    }
    const chunk = samples.slice(start, end);
    pieces.push({ startSec: start / RATE, samples: chunk, silent: rms(chunk, 0, chunk.length) < 0.004 });
    start = end;
  }
  return pieces;
}

/** 区間を順に送る。途中で失敗したら、その区間の理由を返す（送れた分は残る）。 */
export async function uploadPieces(
  meetingId: number,
  baseSec: number,
  pieces: Piece[],
  onProgress: (done: number) => void,
  signal?: AbortSignal,
): Promise<void> {
  for (let index = 0; index < pieces.length; index++) {
    if (signal?.aborted) throw new Error("取り込みを中止しました（送った分は残っています）");
    const piece = pieces[index];
    const wav = encodeWav(piece.samples, RATE);
    await request(`/api/meetings/${meetingId}/audio`, {
      method: "POST",
      headers: {
        "Content-Type": "audio/wav",
        "X-Start-Ms": String(Math.round((baseSec + piece.startSec) * 1000)),
        "X-Silent": piece.silent ? "1" : "0",
        "X-Import": "1",
      },
      raw: wav as unknown as BodyInit,
      signal,
    });
    onProgress(index + 1);
  }
}

/** "01:02:03.500" / "02:03,5" / "62" などを秒にする。 */
function parseTime(text: string): number | null {
  const parts = text.trim().replace(",", ".").split(":");
  if (!parts.length || parts.some((part) => part === "" || Number.isNaN(Number(part)))) return null;
  return parts.reduce((total, part) => total * 60 + Number(part), 0);
}

export type TextLine = { start_sec: number; text: string };

/**
 * 文字起こし済みのテキストを区間に分ける。
 * - WebVTT / SRT: 各キューの開始時刻で。
 * - 行頭に [00:12] や 00:12 の時刻があれば、その時刻で。
 * - 時刻が無ければ、空行で区切った段落ごとに 10 秒ずつずらして並べる（相対的な順序だけを残す）。
 */
export function parseTranscript(source: string): { lines: TextLine[]; timed: boolean } {
  const text = source.replace(/^﻿/, "").replace(/\r\n?/g, "\n");
  const cue = /(\d{1,2}:)?\d{1,2}:\d{2}[.,]\d{1,3}\s*-->\s*(\d{1,2}:)?\d{1,2}:\d{2}[.,]\d{1,3}/;
  if (cue.test(text)) {
    const lines: TextLine[] = [];
    for (const block of text.split(/\n\s*\n/)) {
      const rows = block.split("\n").map((row) => row.trim()).filter(Boolean);
      const at = rows.findIndex((row) => cue.test(row));
      if (at < 0) continue;
      const start = parseTime(rows[at].split("-->")[0]);
      const body = rows
        .slice(at + 1)
        .join(" ")
        .replace(/<[^>]+>/g, "")
        .trim();
      if (start != null && body) lines.push({ start_sec: start, text: body });
    }
    return { lines, timed: true };
  }
  const stamped = /^\[?((?:\d{1,2}:)?\d{1,2}:\d{2}(?:[.,]\d+)?)\]?\s*(.*)$/;
  const rows = text.split("\n").map((row) => row.trim()).filter(Boolean);
  if (rows.length && rows.filter((row) => stamped.test(row)).length >= rows.length / 2) {
    const lines: TextLine[] = [];
    for (const row of rows) {
      const match = row.match(stamped);
      if (match) {
        const start = parseTime(match[1]);
        if (start != null && match[2].trim()) lines.push({ start_sec: start, text: match[2].trim() });
      } else if (lines.length) {
        lines[lines.length - 1].text += ` ${row}`;
      }
    }
    return { lines, timed: true };
  }
  const paragraphs = text
    .split(/\n\s*\n/)
    .map((block) => block.replace(/\n/g, " ").trim())
    .filter(Boolean);
  return { lines: paragraphs.map((body, index) => ({ start_sec: index * 10, text: body })), timed: false };
}

export const AUDIO_ACCEPT = "audio/*,.mp3,.m4a,.wav,.ogg,.aac,.webm,.mp4";
export const TEXT_ACCEPT = ".txt,.vtt,.srt,text/plain,text/vtt";
