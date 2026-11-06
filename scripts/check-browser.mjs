import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { connect, until, delay } from './browser-session.mjs';
const browser = await connect(Number(process.env.CHROME_PORT || 9333));
const report = [];
const assertPage = async (expression, message) => {
  assert.ok(await browser.evaluate(expression), message);
  report.push(message);
};
try {
  await browser.send('Page.enable');
  await browser.send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
    window.resources = { urls: new Set(), peak: 0, reads: [] };
    const create = URL.createObjectURL.bind(URL), revoke = URL.revokeObjectURL.bind(URL);
    URL.createObjectURL = (blob) => { const url = create(blob); resources.urls.add(url); resources.peak = Math.max(resources.peak, resources.urls.size); return url; };
    URL.revokeObjectURL = (url) => { resources.urls.delete(url); return revoke(url); };
    const get = IDBObjectStore.prototype.get;
    IDBObjectStore.prototype.get = function(key) { resources.reads.push(key); return get.call(this, key); };
  `,
  });
  await browser.send('Page.navigate', { url: process.env.CUTLINE_URL || 'http://127.0.0.1:1420/' });
  await delay(250);
  await until(() =>
    browser.evaluate("!!document.querySelector('#root')?.children.length"),
  );
  await browser.evaluate(`(async () => {
    const {storeMedia} = await import('/src/cache.ts');
    const file = await (await fetch('/demo/coast.mp4')).blob();
    const now = Date.now();
    const clips = Array.from({length:40}, (_, i)=>({id:'check-'+i,name:'Saved clip '+String(i).padStart(2,'0'),kind:'video',duration:12,size:file.size,addedAt:now,hasAudio:false}));
    for (const clip of clips) await storeMedia(clip.id, file);
    const timeline = clips.slice(0,3).map((clip,i)=>({id:'check-seg-'+i,clipId:clip.id,in:1,out:2}));
    localStorage.setItem('cutline.projects.v1',JSON.stringify([{id:'check',name:'Browser checks',description:'',createdAt:now,updatedAt:now,clips,timeline,messages:[]}]));
    location.hash='project=check';
  })()`);
  await browser.send('Page.reload');
  await delay(250);
  const ready = () =>
    browser.evaluate(
      "!!document.querySelector('video') && document.querySelector('video').readyState >= 2",
    );
  await until(ready);
  await assertPage(
    'resources.urls.size === 1 && resources.reads.length <= 2',
    'Opening 40 saved clips loads only the selected source',
  );
  await browser.evaluate(
    "document.querySelectorAll('.viewer-tabs button')[1].click()",
  );
  await until(() => browser.evaluate('resources.urls.size === 2'));
  await assertPage(
    'resources.peak <= 2',
    'Sequence retains at most the current and next source',
  );
  await browser.evaluate(
    'document.querySelector(\'[aria-label="Play preview"]\').click()',
  );
  await until(() =>
    browser.evaluate(
      '!!document.querySelector(\'[aria-label="Play preview"]\') && Number(document.querySelector(\'[aria-label="Viewer playhead"]\').value) >= 2.9',
    ),
  );
  await assertPage(
    "document.querySelector('video').paused",
    'Three trimmed clips play through and stop at the end',
  );
  await assertPage(
    'resources.peak <= 2',
    'Crossing cuts keeps the two-source limit',
  );
  await browser.evaluate(
    "window.previousVideo = document.querySelector('video'); document.querySelectorAll('.viewer-tabs button')[0].click(); document.querySelectorAll('.clip-card')[15].click(); document.querySelectorAll('.clip-card')[29].click()",
  );
  await until(ready);
  await assertPage(
    'resources.urls.size === 1',
    'Switching sources releases previous handles',
  );
  await assertPage(
    'previousVideo.paused && !previousVideo.hasAttribute("src")',
    'Switching sources releases the detached decoder',
  );
  await browser.evaluate(
    'document.querySelector(\'[aria-label="Play preview"]\').click()',
  );
  await until(() =>
    browser.evaluate("!document.querySelector('video').paused"),
  );
  await browser.evaluate(
    "Object.defineProperty(document,'hidden',{configurable:true,get:()=>true}); document.dispatchEvent(new Event('visibilitychange'))",
  );
  await until(() => browser.evaluate("document.querySelector('video').paused"));
  report.push('Hiding the editor pauses playback');
  await browser.evaluate('delete document.hidden');
  const { root } = await browser.send('DOM.getDocument');
  const { nodeId } = await browser.send('DOM.querySelector', {
    nodeId: root.nodeId,
    selector: 'input[type=file]',
  });
  await browser.send('DOM.setFileInputFiles', {
    nodeId,
    files: [
      'public/demo/coast.mp4',
      'public/demo/forest.mp4',
      'public/demo/lake.mp4',
      'README.md',
    ].map((p) => resolve(p)),
  });
  await until(() =>
    browser.evaluate("document.querySelectorAll('.clip-card').length === 43"),
  );
  await until(ready);
  await assertPage(
    "JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].clips.slice(-3).map(c=>c.name).join(',') === 'coast.mp4,forest.mp4,lake.mp4'",
    'Batch import preserves file order and isolates unsupported files',
  );
  await assertPage(
    'resources.urls.size === 1',
    'Import releases temporary decoder URLs',
  );
  await assertPage(
    "JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].clips.slice(-3).every(c=>c.thumbnail?.startsWith('data:image/jpeg'))",
    'Asynchronous thumbnail encoding produces saved thumbnails',
  );
  await assertPage(
    "JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].clips.slice(-3).every(c=>c.hasAudio === false)",
    'Silent imports are identified as video only',
  );
  await browser.evaluate(
    'document.querySelector(\'[aria-label="Back to projects"]\').click()',
  );
  await until(() => browser.evaluate('resources.urls.size === 0'));
  report.push('Leaving the editor releases all media URLs');
  await assertPage(
    `(async () => {
    const {inspectMedia} = await import('/src/cache.ts');
    const abort = new AbortController();
    const file = new File([await (await fetch('/demo/coast.mp4')).blob()], 'cancel.mp4', {type:'video/mp4'});
    const inspection = inspectMedia(file, abort.signal);
    abort.abort();
    try { await inspection; return false; } catch (error) { return error.name === 'AbortError' && resources.urls.size === 0; }
  })()`,
    'Cancelling an inspection releases its decoder and object URL',
  );
  await assertPage(
    `(async () => {
    const {readMedia} = await import('/src/cache.ts');
    const abort = new AbortController(); abort.abort();
    try { await readMedia('check-0', abort.signal); return false; } catch(error) { return error.name === 'AbortError'; }
  })()`,
    'Cancelled storage reads do not load a blob',
  );

  const fixture = 'tests/fixtures/video-with-audio.mp4';
  // The fixture is a two-second color card with an AAC test tone.
  const { readFile } = await import('node:fs/promises');
  const fixtureBytes = (await readFile(fixture)).toString('base64');
  await browser.send('Emulation.setDeviceMetricsOverride', {
    width: 1200, height: 900, deviceScaleFactor: 1, mobile: false,
  });
  await browser.evaluate(`(async () => {
    const {storeMedia} = await import('/src/cache.ts');
    const file = new Blob([Uint8Array.from(atob('${fixtureBytes}'), c=>c.charCodeAt(0))], {type:'video/mp4'});
    await storeMedia('legacy-tone', file);
    const now = Date.now();
    const clips = [
      {id:'legacy-tone',name:'Legacy tone',kind:'video',duration:2,size:file.size,addedAt:now},
      {id:'silent',name:'Silent video',kind:'video',duration:12,source:'/demo/coast.mp4',size:0,addedAt:now},
    ];
    const timeline = [
      {id:'tone-start',clipId:'legacy-tone',in:0.5,out:1.5},
      ...Array.from({length:33}, (_, i)=>({id:'silent-'+i,clipId:'silent',in:0,out:12})),
      {id:'tone-end',clipId:'legacy-tone',in:1,out:2},
    ];
    localStorage.setItem('cutline.projects.v1',JSON.stringify([{id:'long',name:'Long sequence',description:'',createdAt:now,updatedAt:now,clips,timeline,messages:[]}]));
    location.hash='project=long';
  })()`);
  await browser.send('Page.reload');
  await delay(250);
  await until(() => browser.evaluate(
    "document.querySelectorAll('.timeline-segment.audio').length === 2",
  ));
  await assertPage(`(() => {
    const [first, last] = document.querySelectorAll('.timeline-segment.audio');
    const video = document.querySelectorAll('.timeline-segment.video');
    const saved = JSON.parse(localStorage.getItem('cutline.projects.v1'))[0];
    return video.length === 35 && saved.clips[0].hasAudio === true && saved.clips[1].hasAudio === false &&
      saved.createdAt === saved.updatedAt &&
      first.style.cssText === video[0].style.cssText &&
      last.style.cssText === video[34].style.cssText;
  })()`, 'Older projects gain aligned linked audio for both trims of the same video');

  const fitsSequence = `(() => {
    const scroll = document.querySelector('.timeline-scroll');
    const last = Array.from(document.querySelectorAll('.timeline-segment.video')).at(-1);
    return scroll.clientWidth > 0 && scroll.scrollLeft === 0 && scroll.scrollWidth === scroll.clientWidth &&
      last.getBoundingClientRect().right <= scroll.getBoundingClientRect().right;
  })()`;
  await assertPage(fitsSequence, 'Fit shows the entire six-minute sequence');
  const ruler = await browser.evaluate(
    "document.querySelector('.timeline-ruler').getBoundingClientRect().toJSON()",
  );
  const mouse = (type, x, y) => browser.send('Input.dispatchMouseEvent', {
    type, x, y,
    button: type === 'mouseMoved' ? 'none' : 'left',
    buttons: type === 'mouseReleased' ? 0 : 1,
    clickCount: type === 'mouseMoved' ? 0 : 1,
  });
  const middle = Math.round(ruler.left + ruler.width / 2);
  await mouse('mousePressed', middle, ruler.top + 22);
  await mouse('mouseReleased', middle, ruler.top + 22);
  const playheadAt = (x) =>
    `Math.abs(document.querySelector('.timeline-playhead').getBoundingClientRect().left - ${x}) < 2`;
  await assertPage(playheadAt(middle), 'Clicking the ruler moves the playhead');

  const dragStart = Math.round(ruler.left + ruler.width * 0.35);
  const dragEnd = Math.round(ruler.left + ruler.width * 0.62);
  await mouse('mousePressed', dragStart, ruler.top + 22);
  await mouse('mouseMoved', dragEnd, ruler.top + 55);
  await until(() => browser.evaluate(playheadAt(dragEnd)));
  await mouse('mouseReleased', dragEnd, ruler.top + 55);
  await assertPage(
    `(${playheadAt(dragEnd)}) && !document.querySelector('.timeline-segment.selected')`,
    'Dragging from the ruler scrubs across a clip without selecting it',
  );
  await browser.evaluate(`window.playheadLeft = document.querySelector('.timeline-playhead').getBoundingClientRect().left;
    document.querySelector('[aria-label="Zoom in timeline"]').click();`);
  await assertPage(`(() => {
    const scroll = document.querySelector('.timeline-scroll');
    const playhead = document.querySelector('.timeline-playhead');
    return scroll.scrollWidth > scroll.clientWidth && scroll.scrollLeft > 0 &&
      Math.abs(playhead.getBoundingClientRect().left - playheadLeft) < 2;
  })()`, 'Zoom in preserves the visible playhead position');
  // Let Chrome update pointer hit testing after the programmatic scroll.
  await browser.evaluate('new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))');
  await mouse('mousePressed', dragEnd + 4, ruler.top + 10);
  await assertPage(playheadAt(dragEnd), 'Grabbing the playhead off center keeps its position');
  await mouse('mouseMoved', dragEnd - 76, ruler.top + 10);
  await until(() => browser.evaluate(playheadAt(dragEnd - 80)));
  await mouse('mouseReleased', dragEnd - 76, ruler.top + 10);
  await assertPage(playheadAt(dragEnd - 80), 'The playhead handle drags at a scrolled zoom level');
  await browser.evaluate("document.querySelector('.timeline-scroll').scrollLeft = 0");
  await browser.evaluate(`(() => {
    const slider = document.querySelector('[aria-label="Timeline zoom"]');
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(slider, slider.max);
    slider.dispatchEvent(new Event('input', {bubbles:true}));
    slider.dispatchEvent(new Event('change', {bubbles:true}));
  })()`);
  await assertPage(`(() => {
    const scroll = document.querySelector('.timeline-scroll');
    return scroll.scrollWidth > 50_000 && document.querySelectorAll('.timeline-ruler > span').length < 25;
  })()`, 'Maximum zoom provides fine detail with a bounded number of ruler labels');
  await browser.evaluate(`document.querySelector('.timeline-scroll').scrollLeft = 35000`);
  await until(() => browser.evaluate(`(() => {
    const ruler = document.querySelector('.timeline-ruler');
    return parseFloat(ruler.firstElementChild.style.left) > 34_000;
  })()`));
  await browser.evaluate(`document.querySelector('[aria-label="Timeline zoom"]').focus()`);
  await browser.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Home', code: 'Home', windowsVirtualKeyCode: 36 });
  await browser.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Home', code: 'Home', windowsVirtualKeyCode: 36 });
  await assertPage(fitsSequence, 'The zoom slider minimum returns to Fit after scrolling');
  await browser.evaluate(`document.querySelector('[aria-label="Zoom in timeline"]').click()`);
  await browser.evaluate(`document.querySelector('[aria-label="Fit sequence to timeline"]').click()`);
  await assertPage(fitsSequence, 'The Fit button restores the whole sequence');
  await browser.send('Emulation.setDeviceMetricsOverride', {
    width: 620, height: 844, deviceScaleFactor: 1, mobile: false,
  });
  await until(() => browser.evaluate(fitsSequence));
  await assertPage(`(${fitsSequence}) && document.querySelector('.timeline-tools').getBoundingClientRect().right <= innerWidth`,
    'Fit recalculates and the toolbar remains visible on a narrow viewport');

  const documentNode = (await browser.send('DOM.getDocument')).root.nodeId;
  const fileInput = await browser.send('DOM.querySelector', {
    nodeId: documentNode, selector: 'input[type=file]',
  });
  await browser.send('DOM.setFileInputFiles', { nodeId: fileInput.nodeId, files: [resolve(fixture)] });
  await until(() => browser.evaluate("document.querySelector('.clip-card.selected')?.title === 'video-with-audio.mp4'"));
  await browser.evaluate("document.body.dispatchEvent(new KeyboardEvent('keydown', {key:'v',bubbles:true}))");
  await assertPage(`(() => {
    const clip = JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].clips.at(-1);
    const audio = document.querySelector('.timeline-segment.audio.selected');
    const video = document.querySelector('.timeline-segment.video.selected');
    return clip.hasAudio === true && audio?.title.includes(clip.name) && audio.style.cssText === video?.style.cssText;
  })()`, 'New video imports with audio insert both linked parts together');
  await browser.evaluate("document.querySelector('.timeline-segment.audio.selected').click()");
  await until(ready);
  await browser.evaluate(`document.querySelector('[aria-label="Play preview"]').click()`);
  await until(() => browser.evaluate(`(() => {
    const video = document.querySelector('video');
    return !video.paused && !video.muted && video.webkitAudioDecodedByteCount > 0;
  })()`));
  await assertPage("document.querySelectorAll('video, audio').length === 1",
    'Sequence preview decodes embedded audio once through the video element');
  await browser.evaluate(`document.querySelector('[aria-label="Remove selected segment"]').click()`);
  await assertPage(`document.querySelectorAll('.timeline-segment.audio').length === 2 &&
    ![...document.querySelectorAll('.timeline-segment')].some(segment => segment.title.includes('video-with-audio.mp4'))`,
    'Removing the linked audio selection removes both parts of the edit');
  await browser.evaluate(`document.querySelector('[aria-label="Undo timeline edit"]').click()`);
  await assertPage("document.querySelectorAll('.timeline-segment.audio').length === 3",
    'Undo restores the video and linked audio together');

  const splitButton = `document.querySelector('[aria-label="Split at playhead"]')`;
  const sendKey = (key, modifiers = {}) => browser.evaluate(
    `document.body.dispatchEvent(new KeyboardEvent('keydown', ${JSON.stringify({ key, bubbles: true, ...modifiers })}))`,
  );
  await browser.evaluate("[...document.querySelectorAll('.timeline-segment.audio')].at(-1).click()");
  const viewerPlayhead = `document.querySelector('[aria-label="Viewer playhead"]')`;
  const cut = Number(await browser.evaluate(`${viewerPlayhead}.value`)) + 0.5;
  await assertPage(`${splitButton}.disabled`, 'Split is disabled at an existing cut');
  await browser.evaluate(`(() => {
    const slider = ${viewerPlayhead};
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(slider, ${cut});
    slider.dispatchEvent(new Event('input', {bubbles: true}));
    slider.dispatchEvent(new Event('change', {bubbles: true}));
  })()`);
  await browser.evaluate(`${splitButton}.click()`);
  const splitMatches = `(() => {
    const saved = JSON.parse(localStorage.getItem('cutline.projects.v1'))[0];
    const [left, right] = saved.timeline.slice(-2);
    const video = [...document.querySelectorAll('.timeline-segment.video')].slice(-2);
    const audio = [...document.querySelectorAll('.timeline-segment.audio')].slice(-2);
    return left.id === 'tone-end' && left.in === 1 && Math.abs(left.out - 1.5) < 0.001 &&
      right.id !== left.id && right.clipId === left.clipId && right.in === left.out && right.out === 2 &&
      Math.abs(Number(${viewerPlayhead}.value) - ${cut}) < 0.001 &&
      video.every((piece, i) => piece.style.cssText === audio[i].style.cssText) &&
      video[1].ariaPressed === 'true' && audio[1].ariaPressed === 'true';
  })()`;
  await assertPage(splitMatches, 'Split at playhead preserves the trimmed source range and aligns linked audio');
  await sendKey('z', { ctrlKey: true });
  await assertPage(`JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].timeline.at(-1).id === 'tone-end' &&
    document.querySelectorAll('.timeline-segment.audio').length === 3`, 'Undo rejoins both parts of the split');
  await sendKey('x');
  await assertPage(splitMatches, 'X splits at the playhead');
  await sendKey('Delete');
  await assertPage(`(() => {
    const last = JSON.parse(localStorage.getItem('cutline.projects.v1'))[0].timeline.at(-1);
    return last.id === 'tone-end' && Math.abs(last.out - 1.5) < 0.001 &&
      document.querySelectorAll('.timeline-segment.audio').length === 3;
  })()`, 'Delete removes only the selected piece and its linked audio');
  console.log(
    JSON.stringify({ passed: report.length, checks: report }, null, 2),
  );
} finally {
  browser.close();
}
