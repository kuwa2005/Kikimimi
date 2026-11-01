import { useEffect, useRef, useState } from "react";
import { fmt } from "../lib/format";
import { level, localSeconds, recordingHere } from "../recorder/recorder";

// 音量は細かく変わるので、React の再描画を使わずに直接幅を変える。
export function Meter({ active, label = "入力の音量" }: { active: boolean; label?: string }) {
  const bar = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    const timer = setInterval(() => {
      if (bar.current) bar.current.style.width = active ? Math.min(100, level() * 280) + "%" : "0%";
    }, 120);
    return () => clearInterval(timer);
  }, [active]);
  return (
    <div className="meter" role="img" aria-label={label}>
      <span ref={bar} />
    </div>
  );
}

/** 録音時間。このタブで録音中なら手元の時間、そうでなければサーバの録音長。 */
export function Clock({ meetingId, fallback }: { meetingId: number; fallback: number }) {
  const [, tick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => tick((value) => value + 1), 250);
    return () => clearInterval(timer);
  }, []);
  const seconds = recordingHere(meetingId) ? localSeconds() : fallback;
  return <strong className="clock">{fmt(seconds)}</strong>;
}
