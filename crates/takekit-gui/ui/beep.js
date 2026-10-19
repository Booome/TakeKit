// One shared beeper for both windows (the panel and the veil): a short
// oscillator, synthesised on the spot — no audio assets to ship.
//
// The context is created at page load, not inside the first beep: creating one
// there makes that beep land late (device init and resume() are both async),
// and the countdown is timed by ear — a late first beep shortens the first
// interval and nothing else looks wrong. A hidden window's context can also be
// suspended by the browser, so `wakeAudio` resumes it before a countdown.

let ctx = null;

function audio() {
  if (!ctx) {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    if (!Ctx) return null;
    try {
      ctx = new Ctx();
    } catch (e) {
      return null;
    }
  }
  return ctx;
}

// Called at page load: pay the creation cost while nothing is counting.
audio();

/// Resume the context if the browser suspended it while the veil was hidden.
/// Safe to call repeatedly; the resume is fire-and-forget.
window.wakeAudio = () => {
  const c = audio();
  if (c && c.state === "suspended") c.resume().catch(() => {});
};

/// "running" / "suspended" / "none" — what the countdown probes report, so a
/// late first beep is visible in the log instead of only in the user's ear.
window.audioState = () => {
  const c = audio();
  return c ? c.state : "none";
};

window.beep = (hz, ms) => {
  try {
    const c = audio();
    if (!c) return;
    if (c.state === "suspended") c.resume().catch(() => {});
    const t = c.currentTime;
    const seconds = ms / 1000;
    const osc = c.createOscillator();
    const gain = c.createGain();
    osc.type = "sine";
    osc.frequency.value = hz;
    // A 10ms ramp in and out: switching a gain on and off in one step reads as
    // a click, not a tone.
    gain.gain.setValueAtTime(0, t);
    gain.gain.linearRampToValueAtTime(0.25, t + 0.01);
    gain.gain.setValueAtTime(0.25, t + Math.max(0.02, seconds - 0.01));
    gain.gain.linearRampToValueAtTime(0, t + seconds);
    osc.connect(gain).connect(c.destination);
    osc.start(t);
    osc.stop(t + seconds + 0.02);
  } catch (e) {
    // No audio device, or a browser that refuses: stay silent.
  }
};
