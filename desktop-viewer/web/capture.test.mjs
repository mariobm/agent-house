import assert from 'node:assert/strict';
import { keyboardCapture } from './capture.js';
const sent = [];
let released = 0;
const c = keyboardCapture((...a) => sent.push(a), () => released++, e => e.key.codePointAt(0), true);
const event = (type, code, flags = {}) => c.event({type, code, key:'', ...flags});
assert.equal(event('keydown','MetaLeft',{metaKey:true}),false);
c.setEnabled(true);
event('keydown','MetaLeft',{metaKey:true});
event('keydown','MetaLeft',{metaKey:true});
assert.deepEqual(sent,[[0xffeb,'MetaLeft',true]]);
c.setEnabled(false);
assert.deepEqual(sent.at(-1),[0xffeb,'MetaLeft',false]);
c.setEnabled(true); sent.length=0;
// Raycast emits modifiers sequentially before the full Hyper chord.
event('keydown','ControlLeft',{ctrlKey:true});
event('keydown','AltLeft',{ctrlKey:true,altKey:true});
event('keydown','ShiftLeft',{ctrlKey:true,altKey:true,shiftKey:true});
const full={ctrlKey:true,altKey:true,shiftKey:true,metaKey:true};
event('keydown','MetaLeft',full);
const down = new Set();
for (const [sym,,pressed] of sent) if(pressed) down.add(sym);else down.delete(sym);
assert.deepEqual([...down],[0xffeb]); // no Ctrl/Alt/Shift survives the collapse
assert.equal(event('keydown','KeyK',{...full,key:'˚'}),true);
assert.deepEqual(sent.at(-1),[107,'KeyK',true]);
event('keyup','KeyK',{...full,key:'˚'});
assert.deepEqual(sent.at(-1),[107,'KeyK',false]);
event('keyup','MetaLeft',{ctrlKey:true,altKey:true,shiftKey:true});
assert.deepEqual(sent.at(-1),[0xffeb,'MetaLeft',false]);
const count=sent.length;
event('keyup','ShiftLeft',{ctrlKey:true,altKey:true});
event('keyup','AltLeft',{ctrlKey:true});
event('keyup','ControlLeft');
assert.equal(sent.length,count); // releasing Hyper must not reintroduce modifiers
assert.equal(event('keydown','Escape',{ctrlKey:true,altKey:true}),true);
assert.equal(released,1);
c.setEnabled(true);
event('keydown','KeyK',{...full,key:'K'});
c.setEnabled(false);
assert.deepEqual(sent.slice(-2),[[107,'KeyK',false],[0xffeb,'MetaLeft',false]]);
console.log('Command, Hyper collapse, key release, focus reset and escape passed');

// Linux retains all original modifiers, including right Alt / AltGr.
const linuxSent = [];
let linuxReleased = false;
const linux = keyboardCapture((...a) => linuxSent.push(a), () => linuxReleased = true);
linux.setEnabled(true);
for (const type of ['keydown', 'keyup']) {
  for (const code of ['MetaLeft', 'ControlRight', 'AltRight', 'ShiftRight', 'KeyK', 'CapsLock']) {
    assert.equal(linux.event({type, code, key:'K', ...full}), false);
  }
}
assert.deepEqual(linuxSent, []);
assert.equal(linux.event({type:'keydown', code:'Escape', ctrlKey:true, altKey:true}), true);
assert.equal(linuxReleased, true);
console.log('Linux modifier passthrough and release shortcut passed');
