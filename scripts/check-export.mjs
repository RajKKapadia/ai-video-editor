import assert from 'node:assert/strict';
import { connect, delay, until } from './browser-session.mjs';

const browser = await connect(Number(process.env.CHROME_PORT || 9333));
const url = process.env.CUTLINE_URL || 'http://127.0.0.1:1420/';
const evaluate = browser.evaluate;
const waitFor = (expression) => until(() => evaluate(expression));
async function click(text) {
  const button = `[...document.querySelectorAll('button')].find(button => button.textContent.trim() === ${JSON.stringify(text)})`;
  await waitFor(`!!(${button}) && !(${button}).disabled`);
  await evaluate(`${button}.click()`);
}
let scriptId;

try {
  await browser.send('Page.enable');
  await browser.send('Emulation.setDeviceMetricsOverride', {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await browser.send('Page.navigate', { url: url + '#project=demo-project' });
  await delay(250);
  await waitFor("!!document.querySelector('.home-shell, .editor-shell')");
  await evaluate("localStorage.removeItem('cutline.projects.v1')");
  await browser.send('Page.reload');
  await delay(250);
  await waitFor("!!document.querySelector('.editor-shell')");
  await click('Export');
  assert.equal(await evaluate("document.querySelector('.export-form button[type=submit]').disabled"), true);
  assert.match(await evaluate("document.querySelector('.export-panel').textContent"), /desktop app/);

  ({ identifier: scriptId } = await browser.send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      window.isTauri = true;
      window.exportCalls = [];
      window.exportMock = { mode: 'success', callbacks: {}, listeners: {}, counter: 0 };
      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
      window.__TAURI_INTERNALS__ = {
        metadata: { currentWebview: { label: 'main' }, currentWindow: { label: 'main' } },
        transformCallback(callback) { const id = ++exportMock.counter; exportMock.callbacks[id] = callback; return id; },
        async invoke(command, args) {
          exportCalls.push({ command, args });
          if (command === 'plugin:event|listen') { exportMock.listeners[args.event] = args.handler; return args.handler; }
          if (command === 'plugin:event|unlisten') return null;
          if (command === 'load_project_cache') return JSON.parse(sessionStorage.getItem('mockExportCache'));
          if (command === 'project_cache_exists') return true;
          if (command === 'save_project_cache') { sessionStorage.setItem('mockExportCache', JSON.stringify(args.snapshot)); return true; }
          if (command === 'ai_provider_status') return {active:null, profiles:{}, warning:null};
          if (command === 'inspect_media_paths') return {'/test/original.mp4':{name:'Linked.mp4',size:1000,url:'/demo/coast.mp4'}};
          if (command === 'export_begin') {
            exportMock.id = args.id;
            if (exportMock.mode === 'picker-cancel') return null;
            return { path: '/tmp/cutline-export-check.mp4', sources: args.sources.filter(source => !source.filePath).map(source => source.clipId) };
          }
          if (['export_source_begin', 'export_source_push', 'export_source_finish', 'plugin:opener|reveal_item_in_dir'].includes(command)) return null;
          if (command === 'export_cancel') { exportMock.reject?.('Export cancelled.'); return null; }
          if (command === 'export_finish') {
            if (exportMock.mode === 'failure') throw 'Encoder unavailable. Install an FFmpeg build with libx264.';
            const handler = exportMock.callbacks[exportMock.listeners['video-export-progress']];
            handler({ event: 'video-export-progress', id: 1, payload: { id: 'another-job', stage: 'Wrong export', fraction: .8 } });
            handler({ event: 'video-export-progress', id: 1, payload: { id: args.id, stage: 'Rendering segment 1 of 3', fraction: .45 } });
            return new Promise((resolve, reject) => { exportMock.resolve = resolve; exportMock.reject = reject; });
          }
          if (command.startsWith('waveform_')) throw 'Waveforms disabled in export check';
          throw new Error('Unexpected export mock command ' + command);
        }
      };
    `,
  }));
  await browser.send('Page.reload');
  await delay(250);
  await waitFor("!!document.querySelector('.home-shell, .editor-shell')");
  await evaluate(`(async () => {
    const { storeMedia } = await import('/src/cache.ts');
    const file = await (await fetch('/demo/coast.mp4')).blob();
    await storeMedia('legacy', file);
    const now = Date.now();
    const clips = [
      {id:'linked',name:'Linked.mp4',kind:'video',duration:12,addedAt:now,size:1000,filePath:'/test/original.mp4',hasAudio:false,width:960,height:540},
      {id:'legacy',name:'Legacy.mp4',kind:'video',duration:12,addedAt:now,size:file.size,hasAudio:false},
      {id:'sample',name:'Sample.mp4',kind:'video',duration:12,addedAt:now,size:file.size,source:'/demo/coast.mp4',hasAudio:false}
    ];
    const timeline = [{id:'a',clipId:'linked',in:1,out:2},{id:'b',clipId:'legacy',in:3,out:4},{id:'c',clipId:'sample',in:0,out:1}];
    localStorage.removeItem('cutline.projects.v1');
    sessionStorage.setItem('mockExportCache',JSON.stringify({revision:'first',projects:[{id:'export-check',name:'Export checks',description:'',createdAt:now,updatedAt:now,clips,timeline,messages:[]}]}));
    location.hash='project=export-check';
  })()`);
  await browser.send('Page.reload');
  await delay(250);
  await waitFor("!!document.querySelector('.editor-shell')");
  await click('Export');
  await evaluate(`for (const [selector,value] of [['#export-resolution','1280x720'],['#export-fps','30'],['#export-quality','high']]) {
    const select = document.querySelector(selector);
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(select,value);
    select.dispatchEvent(new Event('change',{bubbles:true}));
  }`);
  await click('Export MP4');
  await waitFor("document.querySelector('[aria-label=\"Video export progress\"]')?.value === .45");
  assert.equal(await evaluate("document.querySelector('.export-form fieldset').disabled"), true);
  assert.equal(await evaluate("document.querySelector('[aria-label=\"Close export section\"]').disabled"), true);
  const begin = await evaluate("exportCalls.find(call=>call.command==='export_begin').args");
  assert.deepEqual(begin.settings, { width: 1280, height: 720, fps: 30, quality: 'high' });
  assert.deepEqual(begin.segments.map(({clipId, in: start, out}) => [clipId,start,out]), [['linked',1,2],['legacy',3,4],['sample',0,1]]);
  assert.equal(begin.sources[0].filePath, '/test/original.mp4');
  assert.equal(await evaluate("exportCalls.filter(call=>call.command==='export_source_finish').length"), 2);
  assert.equal(await evaluate("exportCalls.filter(call=>call.command==='export_source_push').every(call=>call.args.byteLength<=1048576)"), true);
  await evaluate("document.querySelector('.timeline-segment.video').click()");
  await waitFor("!document.querySelector('[aria-label=\"Remove selected segment\"]').disabled");
  await evaluate("document.querySelector('[aria-label=\"Remove selected segment\"]').click()");
  await waitFor("document.querySelectorAll('.timeline-segment.video').length === 2");
  await evaluate("exportMock.resolve('/tmp/cutline-export-check.mp4')");
  await waitFor("!!document.querySelector('.export-result')");
  assert.match(await evaluate("document.querySelector('.export-result').textContent"), /timeline has changed/);
  await click('Show in folder');
  assert.equal(await evaluate("exportCalls.at(-1).command"), 'plugin:opener|reveal_item_in_dir');

  await evaluate("exportMock.mode='picker-cancel'");
  await click('Export again');
  await waitFor("!document.querySelector('.export-progress')");
  assert.equal(await evaluate("!!document.querySelector('.export-result')"), false);
  await evaluate("exportMock.mode='success'");
  await click('Export MP4');
  await waitFor("document.querySelector('[aria-label=\"Video export progress\"]')?.value === .45");
  await click('Cancel export');
  await waitFor("!document.querySelector('.export-progress')");
  assert.match(await evaluate("document.querySelector('.export-panel').textContent"), /Export cancelled/);
  await evaluate("exportMock.mode='failure'");
  await click('Export MP4');
  await waitFor("!!document.querySelector('.export-panel [role=alert]')");
  assert.match(await evaluate("document.querySelector('.export-panel [role=alert]').textContent"), /Encoder unavailable/);
  await evaluate("exportMock.mode='success'");
  await click('Export MP4');
  await waitFor("document.querySelector('[aria-label=\"Video export progress\"]')?.value === .45");
  await evaluate("exportMock.resolve('/tmp/retry.mp4')");
  await waitFor("document.querySelector('.export-result').textContent.includes('/tmp/retry.mp4')");
  console.log('Export settings, saved ranges, linked/sample/legacy media, chunking, progress isolation, timeline snapshot, completion, folder reveal, picker cancellation, render cancellation, failure, and retry passed.');
} finally {
  if (scriptId) await browser.send('Page.removeScriptToEvaluateOnNewDocument', { identifier: scriptId });
  browser.close();
}
