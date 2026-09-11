import RFB from '@novnc/novnc';
const status = document.getElementById('status');
const rfb = new RFB(document.getElementById('desktop'), `ws://${location.host}${location.pathname}stream`);
rfb.scaleViewport = true;
rfb.resizeSession = false;
rfb.addEventListener('connect', () => { window.ipc?.postMessage('connected'); status.textContent = 'Connected · Closing this window leaves the VM running'; });
rfb.addEventListener('disconnect', () => { window.ipc?.postMessage('disconnected'); status.textContent = 'Disconnected. Close this window and run ahvm desktop again.'; });
rfb.addEventListener('securityfailure', () => { status.textContent = 'Desktop connection rejected.'; });
