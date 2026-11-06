import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { connect, until, delay } from './browser-session.mjs';

// Run against a disposable Chrome profile: this replaces its project fixtures.
const browser = await connect(Number(process.env.CHROME_PORT || 9333));
const checks = [];
const check = async (expression, message) => {
  await until(() => browser.evaluate(expression));
  assert.ok(await browser.evaluate(expression), message);
  checks.push(message);
};
const click = (selector) => browser.evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
const key = (value, modifiers = {}) => browser.evaluate(
  `document.body.dispatchEvent(new KeyboardEvent('keydown', ${JSON.stringify({ key: value, bubbles: true, ...modifiers })}))`,
);
const seek = (time) => browser.evaluate(`(() => {
  const slider = document.querySelector('[aria-label="Viewer playhead"]');
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(slider, ${time});
  slider.dispatchEvent(new Event('input', {bubbles:true}));
  slider.dispatchEvent(new Event('change', {bubbles:true}));
})()`);
const saved = `JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].timeline`;
const original = [
  { id: 'first', clipId: 'shared', in: 0.25, out: 1.25 },
  { id: 'middle', clipId: 'coast', in: 1, out: 5 },
  { id: 'last', clipId: 'shared', in: 1, out: 2 },
];
const unchanged = `JSON.stringify(${saved}) === ${JSON.stringify(JSON.stringify(original))}`;
const markTarget = async () => {
  await click('.viewer-tabs button:nth-child(2)');
  await seek(0.5);
  await key('i');
  await seek(5.5);
  await key('o');
};

try {
  await browser.send('Page.enable');
  await browser.send('Emulation.setDeviceMetricsOverride', {
    width: 1280, height: 900, deviceScaleFactor: 1, mobile: false,
  });
  await browser.send('Page.navigate', { url: process.env.CUTLINE_URL || 'http://127.0.0.1:1420/' });
  await delay(250);
  await until(() => browser.evaluate("!!document.querySelector('#root')?.children.length"));
  const tone = (await readFile('tests/fixtures/video-with-audio.mp4')).toString('base64');
  await browser.evaluate(`(async () => {
    const {storeMedia} = await import('/src/cache.ts');
    await storeMedia('shared', new Blob([Uint8Array.from(atob('${tone}'), c=>c.charCodeAt(0))], {type:'video/mp4'}));
    const now = Date.now();
    const clips = [
      {id:'shared',name:'Linked video',kind:'video',duration:2,size:0,addedAt:now,hasAudio:true},
      {id:'coast',name:'Coast',kind:'video',duration:12,size:0,addedAt:now,hasAudio:false,source:'/demo/coast.mp4'},
      {id:'forest',name:'Forest replacement',kind:'video',duration:12,size:0,addedAt:now,hasAudio:false,source:'/demo/forest.mp4'},
    ];
    localStorage.setItem('cutline.projects.v1',JSON.stringify([{id:'ranges',name:'Timeline range checks',description:'',createdAt:now,updatedAt:now,clips,timeline:${JSON.stringify(original)},messages:[]}]));
    location.hash='project=ranges';
  })()`);
  await browser.send('Page.reload');
  await delay(250);
  await until(() => browser.evaluate("document.querySelectorAll('.timeline-segment.video').length === 3"));
  await check(`document.querySelector('[aria-label="Remove selected segment"]').disabled &&
    document.querySelector('[aria-label="Replace timeline selection with source"]').disabled`,
  'Delete and Replace require a clip or marked timeline range');

  await markTarget();
  await check(`document.querySelector('.viewer-tabs button:nth-child(2)').classList.contains('active') &&
    document.querySelector('.timeline-range-selection')?.ariaLabel.includes('00:00:00:12 to 00:00:05:12') &&
    !document.querySelector('[aria-label="Delete marked timeline range"]').disabled`,
  'I/O mark the sequence across cuts and show a five-second range');
  await browser.evaluate("[...document.querySelectorAll('.clip-card')].find(c=>c.title==='Forest replacement').click()");
  await seek(2);
  await key('i');
  await seek(4);
  await key('o');
  await check(`document.querySelector('.timeline-range-selection') !== null &&
    document.querySelector('.marking-controls').textContent.includes('00:00:02:00') &&
    document.querySelector('.marking-controls').textContent.includes('00:00:04:00')`,
  'Source marks and the timeline target remain independent');

  await key('b');
  await check(`(() => {
    const timeline = ${saved};
    return timeline.length === 3 && timeline[0].in === 0.25 && timeline[0].out === 0.75 &&
      timeline[1].clipId === 'forest' && timeline[1].in === 2 && timeline[1].out === 4 &&
      timeline[2].in === 1.5 && timeline[2].out === 2 &&
      !document.querySelector('.timeline-range-selection') &&
      Number(document.querySelector('[aria-label="Viewer playhead"]').value) === 0.5;
  })()`, 'B replaces across cuts with the marked source and moves the playhead to the new edit');
  await check(`(() => {
    const video = document.querySelectorAll('.timeline-segment.video');
    const audio = document.querySelectorAll('.timeline-segment.audio');
    return audio.length === 2 && audio[0].style.cssText === video[0].style.cssText &&
      audio[1].style.cssText === video[2].style.cssText;
  })()`, 'Replacement preserves alignment of the surrounding linked audio');
  await key('z', { ctrlKey: true });
  await check(unchanged, 'Undo restores the exact pre-replacement timeline');
  await click('.viewer-tabs button:first-child');
  await check(`document.querySelector('.marking-controls').textContent.includes('00:00:02:00') &&
    document.querySelector('.marking-controls').textContent.includes('00:00:04:00')`,
  'Undo retains the independent source selection');

  await markTarget();
  await key('Delete');
  await check(`(() => {
    const timeline = ${saved};
    return timeline.length === 2 && timeline[0].out === 0.75 && timeline[1].in === 1.5 &&
      document.querySelectorAll('.timeline-segment.audio').length === 2;
  })()`, 'Delete ripples a marked range across cuts and removes linked audio together');
  await key('z', { ctrlKey: true });
  await check(unchanged, 'Undo restores a deleted range');

  await seek(0.5);
  await click('[aria-label="Mark timeline in"]');
  await seek(5.5);
  await click('[aria-label="Mark timeline out"]');
  await click('[aria-label="Clear timeline marks"]');
  await check(`!document.querySelector('.timeline-range-selection')`, 'Timeline buttons mark and clear ranges');

  await click('.timeline-segment.video');
  await seek(6);
  await key('i');
  await check(`document.querySelector('[aria-label="Delete marked timeline range"]').disabled`,
    'An empty marked range blocks edits instead of deleting a selected clip');
  await key('Delete');
  await check(unchanged, 'Invalid range deletion leaves all clips unchanged');
  await key('p');
  await click('[aria-label="Replace timeline selection with source"]');
  await check(`${saved}[0].clipId === 'forest' && ${saved}[0].in === 2 && ${saved}[0].out === 4`,
    'Replace falls back to the selected clip after marks are cleared');
  await key('z', { ctrlKey: true });
  await check(unchanged, 'Undo restores replacement of a selected clip');

  await seek(0);
  await key('i');
  await seek(6);
  await key('o');
  await key('Backspace');
  await check(`${saved}.length === 0 && document.querySelectorAll('.timeline-segment').length === 0`,
    'Backspace can remove the whole marked sequence');
  await key('z', { ctrlKey: true });
  await check(unchanged, 'Undo recovers an emptied sequence');

  await markTarget();
  const fits = `(() => {
    const bar = document.querySelector('.timeline-editbar').getBoundingClientRect();
    const actions = [...document.querySelectorAll('.timeline-edit-actions button')];
    const scroll = document.querySelector('.timeline-scroll');
    return actions.every(b=>b.getBoundingClientRect().right <= innerWidth && b.getBoundingClientRect().top >= bar.top) &&
      scroll.scrollWidth === scroll.clientWidth && document.documentElement.scrollWidth <= innerWidth;
  })()`;
  for (const width of [1280, 620]) {
    await browser.send('Emulation.setDeviceMetricsOverride', { width, height: 900, deviceScaleFactor: 1, mobile: false });
    await check(fits, `Timeline range controls fit a ${width}px viewport`);
  }
  await browser.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
  await delay(100);
  if (process.env.TIMELINE_SCREENSHOT) {
    const screenshot = await browser.send('Page.captureScreenshot', { format: 'png' });
    await writeFile(process.env.TIMELINE_SCREENSHOT, Buffer.from(screenshot.data, 'base64'));
  }
  console.log(JSON.stringify({ passed: checks.length, checks }, null, 2));
} finally {
  browser.close();
}
