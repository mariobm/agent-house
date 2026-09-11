import { modifierRepair } from './modifiers.js';
import { keyboardCapture } from './capture.js';
import RFB from '@novnc/novnc';
import { getKeysym } from './node_modules/@novnc/novnc/core/input/util.js';
const status = document.getElementById('status');
const hint = document.getElementById('capture-hint');
const rfb = new RFB(document.getElementById('desktop'), `ws://${location.host}${location.pathname}stream`);
rfb.scaleViewport = true;
rfb.resizeSession = false;
let connected = false;
let capturing = false;
const modifiers = modifierRepair((...args) => rfb.sendKey(...args));
const isMac = /Mac/.test(navigator.platform);
const capture = keyboardCapture((...args) => rfb.sendKey(...args), release, getKeysym, isMac);
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
  hint.textContent = 'Keyboard not captured';
  window.ipc?.postMessage('capture:off');
}
window.ahvmCaptureState = mode => {
  if (mode === 'off' || !connected || !document.hasFocus()) {
    // No IPC echo: the native side has already released its capture token.
    capturing = false; resetKeyboard();
    hint.textContent = 'Keyboard not captured';
    return;
  }
  resetKeyboard();
  capturing = true;
  capture.setEnabled(true);
  const limited = isMac
    ? ' · macOS shortcuts need Accessibility permission for AHVM Desktop'
    : ' · system shortcuts may stay on the host';
  const shortcuts = isMac ? 'Command / Hyper = Super · Ctrl+Option+Esc to release' : 'Super = Super · Ctrl+Alt+Esc to release';
  hint.textContent = `Keyboard focused · ${shortcuts}${mode === 'native' ? '' : limited}`;
  rfb.focus();
};
function focusCapture() {
  if (connected && document.hasFocus() && !capturing) window.ipc?.postMessage('capture:on');
}
window.addEventListener('focus', focusCapture);
document.getElementById('desktop').addEventListener('pointerdown', focusCapture);
rfb.addEventListener('connect', () => {
  connected = true; focusCapture();
  window.ipc?.postMessage('connected'); status.textContent = 'Connected · Closing this window leaves the VM running';
});
rfb.addEventListener('disconnect', () => {
  connected = false; release();
  window.ipc?.postMessage('disconnected'); status.textContent = 'Disconnected. Close this window and run ahvm desktop again.';
});
rfb.addEventListener('securityfailure', () => { release(); status.textContent = 'Desktop connection rejected.'; });
for (const name of ['keydown', 'keyup']) document.addEventListener(name, e => {
  if (capture.event(e)) { e.preventDefault(); e.stopImmediatePropagation(); return; }
  if (!capturing) modifiers.event(e);
}, true);
document.addEventListener('pointerdown', e => { if (!capturing) modifiers.event(e); }, true);
document.addEventListener('pointerup', e => { if (!capturing) modifiers.event(e); }, true);
window.addEventListener('blur', () => { if (!resetting) release(); });
