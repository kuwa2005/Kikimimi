import { memo, useEffect, useLayoutEffect, useRef } from "react";
import type { Segment } from "../api/types";
import { fmt } from "../lib/format";
import { captions } from "../lib/captions";
import { createStore, useStore } from "../lib/store";
import { pref, PREF_NEWEST_FIRST, savePref } from "../lib/prefs";

/** 新しい発言を上に出すか（会議中に切り替えられる。端末ごとに覚える）。 */
export const newestFirst = createStore<boolean>(pref(PREF_NEWEST_FIRST) === "1");

export function setNewestFirst(value: boolean) {
  savePref(PREF_NEWEST_FIRST, value ? "1" : "0");
  newestFirst.set(value);
}

/** 並び順の切り替え。会議中の書き起こしの上に出す。 */
export function OrderToggle() {
  const top = useStore(newestFirst);
  return (
    <div className="transcript-tools">
      <button
        className="btn small ghost"
        type="button"
        aria-pressed={top}
        onClick={() => setNewestFirst(!top)}
        title="書き起こしと速報字幕の並び順を切り替えます"
      >
        {top ? "↑ 新しい発言が上（切り替え）" : "↓ 新しい発言が下（切り替え）"}
      </button>
    </div>
  );
}

/** 速報字幕（#104）。AI の書き起こしがまだ届いていない部分だけを、末尾に薄く出す。保存はしない。 */
export function LiveCaptions({ after, reverse = false }: { after: number; reverse?: boolean }) {
  const state = useStore(captions);
  const lines = state.lines.filter((line) => line.chunkStart > after + 0.05);
  if (!state.running && !lines.length && !state.problem) return null;
  const rows = lines.map((line) => (
    <p key={line.chunkStart} className="seg caption">
      <span className="badge">速報</span>
      <span className="seg-text">{line.text}</span>
    </p>
  ));
  const interim = state.interim ? (
    <p key="interim" className="seg caption interim">
      <span className="badge">速報</span>
      <span className="seg-text">{state.interim}</span>
    </p>
  ) : null;
  return (
    <div className="captions" aria-live="polite" aria-label="速報字幕">
      {reverse ? (
        <>
          {interim}
          {rows.reverse()}
        </>
      ) : (
        <>
          {rows}
          {interim}
        </>
      )}
      {state.problem ? <p className="muted small">{state.problem}</p> : null}
    </div>
  );
}

function Highlight({ text, query }: { text: string; query: string }) {
  if (!query) return <>{text}</>;
  const parts = text.split(query);
  return (
    <>
      {parts.map((part, index) => (
        <span key={index}>
          {part}
          {index < parts.length - 1 ? <mark>{query}</mark> : null}
        </span>
      ))}
    </>
  );
}

// 区間ごとに作り、変わっていない区間は描き直さない（選択中の文字が消えないように）。
const SegmentRow = memo(function SegmentRow({
  segment,
  query,
  seekable,
  playing,
  important,
  onSeek,
  onEdit,
}: {
  segment: Segment;
  query: string;
  seekable: boolean;
  playing: boolean;
  important: boolean;
  onSeek: (sec: number) => void;
  onEdit: (segment: Segment) => void;
}) {
  return (
    <p className={`seg ${playing ? "playing" : ""} ${important ? "important" : ""}`} id={`seg-${segment.id}`} data-start={segment.start_sec}>
      {seekable ? (
        <button type="button" className="ts" onClick={() => onSeek(segment.start_sec)} title="ここから再生" aria-label={`${fmt(segment.start_sec)} から再生`}>
          {fmt(segment.start_sec)}
        </button>
      ) : (
        <time>{fmt(segment.start_sec)}</time>
      )}
      <span className="seg-text">
        {segment.speaker ? <span className="speaker">{segment.speaker}</span> : null}
        {important ? (
          <span className="star" title="「ここ重要」の印" aria-label="重要">
            ★{" "}
          </span>
        ) : null}
        <Highlight text={segment.text} query={query} />
      </span>
      <button type="button" className="mini" onClick={() => onEdit(segment)} aria-label={`${fmt(segment.start_sec)} の書き起こしを直す`} title="書き起こしを直す">
        ✎
      </button>
    </p>
  );
});

function selectionInside(node: HTMLElement | null): boolean {
  const selection = window.getSelection();
  if (!node || !selection || selection.isCollapsed || !selection.rangeCount) return false;
  return node.contains(selection.getRangeAt(0).commonAncestorContainer);
}

export const Transcript = memo(function Transcript({
  segments,
  query,
  seekable,
  playingId,
  flashId,
  onSeek,
  onEdit,
  follow,
  marked,
  live = false,
}: {
  segments: Segment[];
  query: string;
  seekable: boolean;
  playingId: number | null;
  flashId: number | null;
  onSeek: (sec: number) => void;
  onEdit: (segment: Segment) => void;
  follow: boolean;
  /** 「ここ重要」の印が付いた区間。 */
  marked?: Set<number>;
  /** 会議中なら速報字幕を末尾に出す。 */
  live?: boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const captionState = useStore(captions);
  const captionCount = live ? captionState.lines.length + (captionState.interim ? 1 : 0) : 0;
  const nearBottom = useRef(true);
  const nearTop = useRef(true);
  const reverse = useStore(newestFirst) && live;

  // 最新の側を見ているときだけ最新へ追いかける（新しい発言が上なら先頭、下なら末尾）。文字を選んでいる間は動かさない。
  useLayoutEffect(() => {
    const node = box.current;
    if (!node || !follow || query || selectionInside(node)) return;
    if (reverse) {
      if (nearTop.current) node.scrollTop = 0;
    } else if (nearBottom.current) node.scrollTop = node.scrollHeight;
  }, [segments.length, follow, query, captionCount, reverse]);

  useEffect(() => {
    if (flashId == null) return;
    const row = document.getElementById(`seg-${flashId}`);
    if (row) {
      row.scrollIntoView({ block: "center", behavior: "smooth" });
      row.classList.add("flash");
      const timer = setTimeout(() => row.classList.remove("flash"), 1800);
      return () => clearTimeout(timer);
    }
  }, [flashId]);

  const shown = query ? segments.filter((segment) => segment.text.includes(query)) : segments;
  return (
    <div
      className="body transcript"
      ref={box}
      onScroll={(event) => {
        const node = event.currentTarget;
        nearBottom.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
        nearTop.current = node.scrollTop < 48;
      }}
      tabIndex={0}
      aria-label="書き起こし"
    >
      {live ? <OrderToggle /> : null}
      {reverse && !query ? <LiveCaptions after={segments.length ? segments[segments.length - 1].start_sec : -1} reverse /> : null}
      {!segments.length ? (
        <div className="empty">話し始めると、数秒遅れてここに出ます。</div>
      ) : !shown.length ? (
        <div className="empty">「{query}」を含む発言はありません。</div>
      ) : (
        (reverse ? [...shown].reverse() : shown).map((segment) => (
          <SegmentRow
            key={segment.id}
            segment={segment}
            query={query}
            seekable={seekable}
            playing={segment.id === playingId}
            important={!!marked?.has(segment.id)}
            onSeek={onSeek}
            onEdit={onEdit}
          />
        ))
      )}
      {live && !reverse && !query ? <LiveCaptions after={segments.length ? segments[segments.length - 1].start_sec : -1} /> : null}
    </div>
  );
});
