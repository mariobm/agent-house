import assert from 'node:assert/strict';
import { modifierRepair } from './modifiers.js';
const sent = [];
const repair = modifierRepair((...args) => sent.push(args));
repair.event({ type: 'keydown', code: 'KeyC', ctrlKey: true });
repair.event({ type: 'keyup', code: 'KeyC', ctrlKey: true });
assert.deepEqual(sent, [[0xffe3, 'ControlLeft', true]]);
repair.event({ type: 'keydown', code: 'KeyT', ctrlKey: false });
assert.deepEqual(sent.pop(), [0xffe3, 'ControlLeft', false]);
sent.length = 0;
repair.event({ type: 'keydown', code: 'ControlLeft', ctrlKey: true });
repair.event({ type: 'keydown', code: 'KeyC', ctrlKey: true });
repair.event({ type: 'keyup', code: 'ControlLeft', ctrlKey: false });
assert.deepEqual(sent, []); // Physical modifier events already reach noVNC.
repair.event({ type: 'keydown', code: 'KeyA', shiftKey: true });
repair.reset();
assert.deepEqual(sent, [[0xffe1, 'ShiftLeft', true], [0xffe1, 'ShiftLeft', false]]);
console.log('modifier repair: missing events, physical events and blur release pass');

sent.length = 0;
repair.event({ type: 'keydown', code: 'KeyE', ctrlKey: true, getModifierState: key => key === 'AltGraph' });
assert.deepEqual(sent, []);
repair.event({ type: 'keydown', code: 'KeyC', ctrlKey: true });
repair.event({ type: 'pointerdown', ctrlKey: false });
assert.deepEqual(sent, [[0xffe3, 'ControlLeft', true], [0xffe3, 'ControlLeft', false]]);
