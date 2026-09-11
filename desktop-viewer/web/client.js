import { modifierRepair } from './modifiers.js';
import { keyboardCapture } from './capture.js';
import RFB from '@novnc/novnc';
const status = document.getElementById('status');
const button = document.getElementById('capture');
const hint = document.getElementById('capture-hint');
const rfb = new RFB(document.getElementById('desktop'), `ws://${location.host}${location.pathname}stream`);
rfb.scaleViewport = true;
rfb.resizeSession = false;
let connected = false;
let capturing = false;
const modifiers = modifierRepair((...args) => rfb.sendKey(...args));
const capture = keyboardCapture((...args) => rfb.sendKey(...args), release);
let resetting = false;
function resetKeyboard() {
  capture.setEnabled(false);
  modifiers.reset();
  // noVNC releases every held key on window blur. Canvas.blur alone does not.
  resetting = true;
  window.dispatchEvent(new Event('blur'));
  rfb.blur();
  resetting = false;
}
function release() {
  capturing = false;
  resetKeyboard();
  button.textContent = 'Capture keyboard';
  button.setAttribute('aria-pressed', 'false');
  hint.textContent = 'Keyboard not captured';
  window.ipc?.postMessage('capture:off');
}
window.ahvmCaptureState = mode => {
  if (mode === 'off' || !connected || !document.hasFocus()) {
    // No IPC echo: the native side has already released its capture token.
    capturing = false; resetKeyboard();
    button.textContent = 'Capture keyboard'; button.setAttribute('aria-pressed', 'false');
    hint.textContent = 'Keyboard not captured';
    return;
  }
  capturing = true;
  capture.setEnabled(true);
  button.textContent = 'Release keyboard';
  button.setAttribute('aria-pressed', 'true');
  const limited = /Mac/.test(navigator.platform)
    ? ' · macOS shortcuts need Accessibility permission for AHVM Desktop'
    : ' · system shortcuts may stay on the host';
  hint.textContent = `Keyboard captured · Ctrl+Alt+Esc to release${mode === 'native' ? '' : limited}`;
  rfb.focus();
};
button.addEventListener('click', () => {
  if (capturing) release();
  else if (connected) window.ipc?.postMessage('capture:on');
});
rfb.addEventListener('connect', () => {
  connected = true; button.disabled = false;
  window.ipc?.postMessage('connected'); status.textContent = 'Connected · Closing this window leaves the VM running';
});
rfb.addEventListener('disconnect', () => {
  connected = false; button.disabled = true; release();
  window.ipc?.postMessage('disconnected'); status.textContent = 'Disconnected. Close this window and run ahvm desktop again.';
});
rfb.addEventListener('securityfailure', () => { release(); status.textContent = 'Desktop connection rejected.'; });
for (const name of ['keydown', 'keyup']) document.addEventListener(name, e => {
  if (capture.event(e)) { e.preventDefault(); e.stopImmediatePropagation(); return; }
  modifiers.event(e);
}, true);
document.addEventListener('pointerdown', e => modifiers.event(e), true);
document.addEventListener('pointerup', e => modifiers.event(e), true);
window.addEventListener('blur', () => { if (!resetting) release(); });
