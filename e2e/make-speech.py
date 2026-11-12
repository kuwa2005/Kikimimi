"""試験用の偽マイク音声（発話と無音が交互に続く 16kHz WAV）を作る。"""
import math, random, struct, sys, wave

out = sys.argv[1] if len(sys.argv) > 1 else "speech.wav"
rate = 16000
random.seed(1)
frames = []
for _ in range(12):
    for i in range(int(rate * random.uniform(1.2, 3.0))):
        t = i / rate
        v = 0.35 * math.sin(2 * math.pi * 220 * t) * (0.6 + 0.4 * math.sin(2 * math.pi * 4 * t)) + 0.08 * random.uniform(-1, 1)
        frames.append(v)
    frames += [0.002 * random.uniform(-1, 1) for _ in range(int(rate * random.uniform(0.7, 1.0)))]
with wave.open(out, "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(b"".join(struct.pack("<h", int(max(-1, min(1, x)) * 32767)) for x in frames))
