// Own modifier state while focused so Raycast Hyper becomes only Linux Super.
export function keyboardCapture(sendKey, releaseRequested, keysymForEvent = () => null, isMac = false) {
  let enabled = false;
  let hyper = false;
  const held = new Map();
  const keys = new Map();
  const modifierCode = /^(Meta|Control|Shift|Alt)(Left|Right)$/;
  function sync(wanted) {
    for (const [code, sym] of held) if (!wanted.has(code)) {
      sendKey(sym, code, false); held.delete(code);
    }
    for (const [code, sym] of wanted) if (!held.has(code)) {
      sendKey(sym, code, true); held.set(code, sym);
    }
  }
  return {
    setEnabled(value) {
      for (const [code, sym] of keys) sendKey(sym, code, false);
      keys.clear(); sync(new Map()); hyper = false; enabled = value;
    },
    event(e) {
      if (!enabled) return false;
      const full = e.ctrlKey && e.altKey && e.shiftKey && e.metaKey;
      if (e.type === 'keydown' && e.code === 'Escape' && e.ctrlKey && e.altKey && !full) {
        this.setEnabled(false); releaseRequested(); return true;
      }
      // Other platforms keep noVNC’s native modifier and AltGr handling.
      if (!isMac) return false;
      const wasHyper = hyper;
      if (full) hyper = true;
      const wanted = new Map();
      if (hyper) {
        if (full) wanted.set('MetaLeft', 0xffeb);
        if (!e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) hyper = false;
      } else {
        if (e.ctrlKey) wanted.set('ControlLeft', 0xffe3);
        if (e.shiftKey) wanted.set('ShiftLeft', 0xffe1);
        if (e.altKey) wanted.set('AltLeft', 0xffe9);
        if (e.metaKey) wanted.set('MetaLeft', 0xffeb);
      }
      sync(wanted);
      if (modifierCode.test(e.code) || (e.code === 'CapsLock' && (full || wasHyper))) return true;
      if (e.type === 'keyup' && keys.has(e.code)) {
        sendKey(keys.get(e.code), e.code, false); keys.delete(e.code); return true;
      }
      if (full && e.type === 'keydown') {
        // Hyper's Shift/Option must not turn K into an uppercase/Option glyph.
        const key = /^Key[A-Z]$/.test(e.code) ? e.code.slice(3).toLowerCase()
          : /^Digit[0-9]$/.test(e.code) ? e.code.slice(5) : e.key;
        const sym = keysymForEvent({key, code: e.code, location: e.location});
        if (sym != null) { keys.set(e.code, sym); sendKey(sym, e.code, true); }
        return true;
      }
      return false;
    },
  };
}
