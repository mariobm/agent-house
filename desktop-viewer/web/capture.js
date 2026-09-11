// Keep macOS Command as Super while captured, bypassing noVNC's Mac Alt swap.
export function keyboardCapture(sendKey, releaseRequested) {
  let enabled = false;
  const held = new Map();
  const releaseMeta = () => {
    for (const [code, sym] of held) sendKey(sym, code, false);
    held.clear();
  };
  return {
    setEnabled(value) { releaseMeta(); enabled = value; },
    event(e) {
      if (!enabled) return false;
      if (e.type === 'keydown' && e.code === 'Escape' && e.ctrlKey && e.altKey) {
        enabled = false;
        releaseMeta();
        releaseRequested();
        return true;
      }
      if (e.code === 'MetaLeft' || e.code === 'MetaRight') {
        const sym = e.code === 'MetaLeft' ? 0xffeb : 0xffec;
        if (e.type === 'keydown' && !held.has(e.code)) {
          held.set(e.code, sym); sendKey(sym, e.code, true);
        } else if (e.type === 'keyup' && held.has(e.code)) {
          sendKey(sym, e.code, false); held.delete(e.code);
        }
        return true;
      }
      // Native input sources sometimes omit the standalone modifier event.
      if (e.metaKey && held.size === 0) {
        held.set('MetaLeft', 0xffeb); sendKey(0xffeb, 'MetaLeft', true);
      } else if (!e.metaKey) releaseMeta();
      return false;
    },
  };
}
