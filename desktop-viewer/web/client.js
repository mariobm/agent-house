import { modifierRepair } from './modifiers.js';
import RFB from '@novnc/novnc';
const status = document.getElementById('status');
const rfb = new RFB(document.getElementById('desktop'), `ws://${location.host}${location.pathname}stream`);
rfb.scaleViewport = true;
rfb.resizeSession = false;
rfb.addEventListener('connect', () => { window.ipc?.postMessage('connected'); status.textContent = 'Connected · Closing this window leaves the VM running'; });
rfb.addEventListener('disconnect', () => { window.ipc?.postMessage('disconnected'); status.textContent = 'Disconnected. Close this window and run ahvm desktop again.'; });
rfb.addEventListener('securityfailure', () => { status.textContent = 'Desktop connection rejected.'; });

const modifiers = modifierRepair((...args) => rfb.sendKey(...args));
document.addEventListener('keydown', e => modifiers.event(e), true);
document.addEventListener('keyup', e => modifiers.event(e), true);
document.addEventListener('pointerdown', e => modifiers.event(e), true);
document.addEventListener('pointerup', e => modifiers.event(e), true);
window.addEventListener('blur', () => modifiers.reset());
rfb.addEventListener('disconnect', () => modifiers.reset());
