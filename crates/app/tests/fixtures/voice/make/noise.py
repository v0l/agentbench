import sys
from pathlib import Path

import numpy as np
from scipy.io import wavfile
from scipy.signal import butter, lfilter, resample_poly

R = 16000
OUT = Path(__file__).resolve().parent.parent
TTS = Path(sys.argv[1]) if len(sys.argv) > 1 else None
rng = np.random.default_rng(1513)


def room(sec):
    return rng.normal(0, 0.0004, int(R * sec))


def band(x, lo, hi, order=2):
    b, a = butter(order, [lo / (R / 2), hi / (R / 2)], btype="band")
    return lfilter(b, a, x)


def save(name, x):
    x = np.clip(x, -1, 1)
    wavfile.write(OUT / f"{name}.wav", R, (x * 32767).astype(np.int16))


def clap():
    out = room(0.5)
    n = int(R * 0.25)
    t = np.arange(n) / R
    burst = rng.normal(0, 1, n) * np.exp(-t / 0.012)
    burst = band(burst, 600, 6000) * 0.9
    tail = rng.normal(0, 1, n) * np.exp(-t / 0.06) * 0.03
    out = np.concatenate([out, burst + tail, room(0.5)])
    return out


def breath():
    n = int(R * 0.9)
    t = np.arange(n) / R
    env = np.sin(np.pi * t / t[-1]) ** 2
    noise = band(rng.normal(0, 1, n), 400, 3000) * env * 0.12
    return np.concatenate([room(0.5), noise, room(0.5)])


def cough():
    parts = [room(0.5)]
    for dur, amp in [(0.22, 0.5), (0.16, 0.3)]:
        n = int(R * dur)
        t = np.arange(n) / R
        env = (1 - np.exp(-t / 0.005)) * np.exp(-t / (dur / 3))
        src = rng.normal(0, 1, n)
        voiced = band(src, 300, 700) * 1.5 + band(src, 1200, 2500) + band(src, 2500, 4000) * 0.5
        parts.append(voiced * env * amp)
        parts.append(room(0.08))
    parts.append(room(0.5))
    return np.concatenate(parts)


def silence():
    t = np.arange(int(R * 3)) / R
    return room(3) + 0.0003 * np.sin(2 * np.pi * 50 * t)


def spoken(name):
    rate, x = wavfile.read(TTS / f"{name}.wav")
    x = x.astype(np.float64) / 32768
    x = resample_poly(x, R, rate)
    nz = np.nonzero(np.abs(x) > 0.01)[0]
    x = x[max(nz[0] - 1600, 0): nz[-1] + 1600]
    return np.concatenate([room(0.5), x + room(len(x) / R), room(0.5)])


save("silence", silence())
save("clap", clap())
save("breath", breath())
save("cough", cough())
if TTS:
    for word in ["yes", "stop"]:
        save(word, spoken(word))
