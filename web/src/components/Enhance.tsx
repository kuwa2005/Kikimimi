import { useStore } from "../lib/store";
import { recorderState, setEnhance } from "../recorder/recorder";

/** 音声強化の切り替え。録音中でもそのまま切り替わる。 */
export function EnhanceToggle() {
  const rec = useStore(recorderState);
  return (
    <label className="enhance-toggle" title="声が小さい・雑音が多いときに。低い雑音を削り、声の大きさをそろえ、音量を自動で持ち上げます">
      <input type="checkbox" checked={rec.enhance} onChange={(event) => setEnhance(event.target.checked)} />
      <span>音声強化</span>
      {rec.enhance && rec.capturing ? <span className="muted small">（音量 ×{rec.gain.toFixed(1)}）</span> : null}
    </label>
  );
}
