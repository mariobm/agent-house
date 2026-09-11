import assert from 'node:assert/strict';
import { keyboardCapture } from './capture.js';
const sent = [];
let released = 0;
const c = keyboardCapture((...a) => sent.push(a), () => released++);
const event = (type, code, flags = {}) => c.event({ type, code, ...flags });
assert.equal(event('keydown', 'MetaLeft'), false);
c.setEnabled(true);
assert.equal(event('keydown', 'MetaLeft'), true);
event('keydown', 'MetaLeft'); // autorepeat must not duplicate modifier downs
assert.deepEqual(sent, [[0xffeb, 'MetaLeft', true]]);
event('keydown', 'MetaRight');
event('keyup', 'MetaLeft');
c.setEnabled(false); // focus loss/disconnect releases either Command key
assert.deepEqual(sent.slice(-2), [[0xffeb, 'MetaLeft', false], [0xffec, 'MetaRight', false]]);
sent.length = 0;
c.setEnabled(true);
event('keydown', 'Space', {metaKey: true}); // missing physical Meta event
assert.deepEqual(sent, [[0xffeb, 'MetaLeft', true]]);
assert.equal(event('keydown', 'Escape', {ctrlKey: true, altKey: true}), true);
assert.equal(released, 1);
assert.deepEqual(sent.at(-1), [0xffeb, 'MetaLeft', false]);
assert.equal(event('keydown', 'MetaLeft'), false);
console.log('Keyboard capture: modifier mapping, repeat, release and escape passed');
