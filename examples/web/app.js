// The page half of the web example.
//
// Everything runs on the main thread, and unlike the muse-rs example there is
// no render worker. That is a data-rate decision, not an oversight: a WHOOP
// sends a heart rate about once a second and a handful of R-R intervals with
// it, so the drawing is a few dozen path operations a second. Moving that to an
// OffscreenCanvas would add a worker, a transfer and a message protocol to save
// nothing measurable. (An EEG headset at 256 Hz across eight channels is the
// case that needs one.)
//
// Web Bluetooth has to be here regardless: `navigator.bluetooth` is on `Window`
// and not on `WorkerNavigator`, and `requestDevice` only opens from a user
// gesture, which only this thread has.
//
// `webbluetooth.js` is the shim from `webbluetooth-wasm`, copied in by
// `build.sh` rather than vendored, so it cannot drift from the module's ABI.
import { instantiate } from './webbluetooth.js';

const connect = document.querySelector('#connect');
const offload = document.querySelector('#offload');
const statusLine = document.querySelector('#status');
const bpmLine = document.querySelector('#bpm');
const rrLine = document.querySelector('#rr');
const batteryLine = document.querySelector('#battery');
const identityLine = document.querySelector('#identity');
const eventsLine = document.querySelector('#events');
const recordsLine = document.querySelector('#records');
const progressLine = document.querySelector('#progress');
const canvas = document.querySelector('#trace');

// Two minutes of beats at a resting rate, which is enough to see a trend
// without turning the trace into a solid block.
const HISTORY = 240;
const beats = [];

const identityParts = [];
const eventLines = [];
const recordLines = [];
const LOG_LINES = 40;

let instance = null;
const decoder = new TextDecoder();

// A string from the module's memory. Built per call: the buffer is detached
// whenever linear memory grows, so a cached view would go stale.
function text(pointer, length) {
  return decoder.decode(new Uint8Array(instance.exports.memory.buffer, pointer, length));
}

function pushLine(store, element, line) {
  store.push(line);
  if (store.length > LOG_LINES) store.shift();
  // Newest first: the interesting line is the one that just arrived, and a log
  // that grows downward makes the reader chase it.
  element.textContent = store.slice().reverse().join('\n');
}

// ── Drawing ─────────────────────────────────────────────────────────────────

// Canvas pixels are device pixels, so the backing store is sized from the CSS
// box and the device ratio, and the context is scaled once per resize.
function resize() {
  const bounds = canvas.getBoundingClientRect();
  const ratio = window.devicePixelRatio || 1;
  canvas.width = Math.max(1, Math.round(bounds.width * ratio));
  canvas.height = Math.max(1, Math.round(bounds.height * ratio));
  draw();
}

function draw() {
  const context = canvas.getContext('2d');
  const { width, height } = canvas;
  context.clearRect(0, 0, width, height);

  if (beats.length < 2) return;

  // The axis is fitted to the data with a margin rather than pinned to a fixed
  // 40–200 bpm: a resting trace inside a fixed range is a flat line, and the
  // variation is the whole reason to draw it.
  let low = Math.min(...beats);
  let high = Math.max(...beats);
  if (high - low < 10) {
    const middle = (high + low) / 2;
    low = middle - 5;
    high = middle + 5;
  }
  const pad = (high - low) * 0.1;
  low -= pad;
  high += pad;

  const x = (i) => (i / (HISTORY - 1)) * width;
  const y = (bpm) => height - ((bpm - low) / (high - low)) * height;

  context.lineWidth = 2 * (window.devicePixelRatio || 1);
  context.strokeStyle = '#f0556b';
  context.beginPath();
  beats.forEach((bpm, i) => (i === 0 ? context.moveTo(x(i), y(bpm)) : context.lineTo(x(i), y(bpm))));
  context.stroke();

  // The range, so the trace is readable as numbers and not just a shape.
  context.fillStyle = '#8a8a94';
  context.font = `${11 * (window.devicePixelRatio || 1)}px ui-monospace, monospace`;
  context.fillText(`${high.toFixed(0)}`, 4, 14 * (window.devicePixelRatio || 1));
  context.fillText(`${low.toFixed(0)}`, 4, height - 4);
}

// ── The module's imports ────────────────────────────────────────────────────

// What `examples/web/lib.rs` declares as its `whoop` import namespace. The shim
// spreads this alongside its own `webbluetooth` namespace.
const imports = {
  whoop: {
    whoop_status(pointer, length) {
      statusLine.textContent = text(pointer, length);
    },

    whoop_identity(pointer, length) {
      identityParts.push(text(pointer, length));
      identityLine.textContent = identityParts.join(' · ');
    },

    whoop_hr(bpm) {
      bpmLine.textContent = bpm;
      beats.push(bpm);
      if (beats.length > HISTORY) beats.shift();
      draw();
    },

    whoop_rr(ms) {
      rrLine.textContent = ms;
    },

    whoop_battery(percent) {
      batteryLine.textContent = percent.toFixed(0);
    },

    whoop_event(number, pointer, length) {
      pushLine(eventLines, eventsLine, `${text(pointer, length)} (${number})`);
    },

    whoop_record(pointer, length) {
      pushLine(recordLines, recordsLine, text(pointer, length));
    },

    whoop_progress(chunks, records) {
      progressLine.textContent = `— ${records} in ${chunks} chunk(s)`;
    },

    // The library's own logs. Silent otherwise in a browser: `env_logger` wants
    // RUST_LOG and stderr, and a page has neither.
    whoop_log(level, pointer, length) {
      const line = text(pointer, length);
      // log::Level is 1..=5. `trace` is mapped to `debug` rather than
      // `console.trace`, which would print a JS stack that means nothing here.
      const method = [, 'error', 'warn', 'info', 'debug', 'debug'][level] || 'log';
      console[method](`[whoop] ${line}`);
    },

    // Bracketed around the whole task on the Rust side, so this is also how the
    // buttons come back after a failure.
    whoop_active(active) {
      const busy = active === 1;
      connect.disabled = busy;
      offload.disabled = busy;
      if (busy) {
        // Cleared when a run starts rather than when one ends: a finished run
        // stays on screen to be read, and a new strap starts from an empty
        // trace instead of splicing onto the last one's beats.
        beats.length = 0;
        identityParts.length = 0;
        draw();
      }
    },
  },
};

// ── Wiring ──────────────────────────────────────────────────────────────────

function fail(message) {
  statusLine.textContent = message;
  connect.disabled = true;
  offload.disabled = true;
}

if (!navigator.bluetooth) {
  // Worth saying plainly: the usual cause is Safari or Firefox, where Web
  // Bluetooth is not implemented, and no amount of retrying will help.
  fail('This browser has no Web Bluetooth. Chrome or Edge, over localhost or HTTPS.');
} else {
  resize();
  new ResizeObserver(resize).observe(canvas);

  instance = await instantiate(fetch('./web.wasm'), imports);

  connect.disabled = false;
  offload.disabled = false;
  statusLine.textContent = 'Ready.';

  // Straight through to the module, still inside the click: the chooser has to
  // open while the gesture is live, which is why neither handler awaits.
  connect.addEventListener('click', () => instance.exports.whoop_connect());
  offload.addEventListener('click', () => instance.exports.whoop_offload());
}
