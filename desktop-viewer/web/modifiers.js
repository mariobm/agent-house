// Some native/accessibility event sources report modifier flags on a letter
// without emitting the modifier's own keydown. Repair only those missing events;
// leave ordinary physical key events to noVNC.
export function modifierRepair(sendKey) {
  const modifiers = [
    { flag: 'ctrlKey', code: 'ControlLeft', keysym: 0xffe3, codes: ['ControlLeft', 'ControlRight'] },
    { flag: 'shiftKey', code: 'ShiftLeft', keysym: 0xffe1, codes: ['ShiftLeft', 'ShiftRight'] },
  ].map(m => ({ ...m, physical: new Set(), synthetic: false }));
  function release(m) {
    if (m.synthetic) sendKey(m.keysym, m.code, false);
    m.synthetic = false;
  }
  return {
    event(e) {
      for (const m of modifiers) {
        if (m.codes.includes(e.code)) {
          release(m);
          if (e.type === 'keydown') m.physical.add(e.code);
          else m.physical.delete(e.code);
        } else if (e[m.flag] && !(m.flag === 'ctrlKey' && e.getModifierState?.('AltGraph')) && m.physical.size === 0) {
          if (!m.synthetic) sendKey(m.keysym, m.code, true);
          m.synthetic = true;
        } else release(m);
      }
    },
    reset() { for (const m of modifiers) { release(m); m.physical.clear(); } },
  };
}
