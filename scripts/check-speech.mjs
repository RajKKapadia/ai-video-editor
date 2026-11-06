import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import { connect, until } from './browser-session.mjs';

const browser = await connect(Number(process.env.CHROME_PORT || 9345));
const evaluate = browser.evaluate;
const waitFor = (expression) => until(() => evaluate(expression));
async function click(text) {
  const target = `Array.from(document.querySelectorAll('button')).find(button => button.textContent.trim() === ${JSON.stringify(text)})`;
  await waitFor(`!!(${target}) && !(${target}).disabled`);
  await evaluate(`${target}.click()`);
}
let scriptId;
try {
  await browser.send('Page.enable');
  ({ identifier: scriptId } = await browser.send('Page.addScriptToEvaluateOnNewDocument', { source: `
    window.isTauri = true;
    window.speechCalls = [];
    window.speechMode = 'success';
    window.speechCallbacks = {};
    window.speechListeners = {};
    window.nextCallback = 0;
    window.speechMock = {
      config: { preference: 'autoBalanced', processing: 'auto' },
      hardware: { cpu: 'Intel Core Ultra 7 265K', cores: 20, totalMemory: 64*1024**3, availableMemory: 44*1024**3, freeDisk: 100*1024**3, devices: [], compiledBackends: ['CUDA'], warning: null },
      selection: { model: 'largeV3Turbo', device: null, threads: 8, reason: 'Auto balances available memory and measured speed.', benchmark: null },
      models: [
        { id: 'small', label: 'Whisper Small', downloadBytes: 487601967, downloaded: false },
        { id: 'medium', label: 'Whisper Medium', downloadBytes: 1533763059, downloaded: false },
        { id: 'largeV3Turbo', label: 'Whisper Large v3 Turbo', downloadBytes: 1624555275, downloaded: false },
        { id: 'largeV3', label: 'Whisper Large v3', downloadBytes: 3095033483, downloaded: true }
      ]
    };
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWebview: { label: 'main' }, currentWindow: { label: 'main' } },
      transformCallback(callback) { const id = ++nextCallback; speechCallbacks[id] = callback; return id; },
      async invoke(command, args) {
        speechCalls.push({ command, args });
        if (command === 'plugin:event|listen') { speechListeners[args.event] = args.handler; return 1; }
        if (command === 'plugin:event|unlisten') return null;
        if (command === 'load_project_cache') return null;
        if (command === 'project_cache_exists') return false;
        if (command === 'save_project_cache') return true;
        if (command === 'speech_status') {
          if (args?.recheck) speechMock.selection.benchmark = null;
          return structuredClone(speechMock);
        }
        if (command === 'speech_save') {
          speechMock.config = args.config;
          speechMock.selection.model = args.config.preference.startsWith('auto') ? 'largeV3Turbo' : args.config.preference;
          speechMock.selection.reason = 'Selected explicitly; the model remains fixed during CPU fallback.';
          return structuredClone(speechMock);
        }
        if (command === 'speech_prepare') {
          speechCallbacks[speechListeners['speech-prepare-progress']]({ payload: { stage: 'Testing Whisper Large v3', fraction: null } });
          if (speechMode === 'cancel') return await new Promise((resolve, reject) => { window.cancelPreparation = () => reject('Preparation cancelled.'); });
          await new Promise(resolve => setTimeout(resolve, 120));
          speechMock.selection.benchmark = { loadSeconds: 2, realtimeFactor: .5, measuredAtMs: Date.now() };
          speechMock.selection.reason = 'GPU testing failed; using CPU with the same model.';
          return structuredClone(speechMock);
        }
        if (command === 'speech_cancel') { window.cancelPreparation?.(); return null; }
        if (command === 'ai_provider_status') return { active: null, profiles: {}, warning: null };
        throw new Error('Unexpected mock command: ' + command);
      }
    };
  ` }));
  await browser.send('Page.navigate', { url: process.env.CUTLINE_URL || 'http://127.0.0.1:1421/' });
  await waitFor("!!document.querySelector('.home-shell')");
  await click('Settings');
  await waitFor("!!document.querySelector('.speech-settings select')");
  assert.equal(await evaluate("document.querySelector('.speech-settings select').value"), 'autoBalanced');
  await evaluate("const select = document.querySelector('.speech-settings select'); select.value = 'largeV3'; select.dispatchEvent(new Event('change', {bubbles:true}));");
  await click('Save and use');
  await waitFor("document.querySelector('.speech-summary strong').textContent === 'Whisper Large v3'");
  assert.equal(await evaluate('speechMock.config.preference'), 'largeV3');
  await click('Download and test');
  await waitFor("document.querySelector('.speech-settings').textContent.includes('transcription test completed')");
  assert.equal(await evaluate("document.querySelector('.speech-settings select').value"), 'largeV3');
  assert.equal(await evaluate("document.querySelector('.speech-summary strong').textContent"), 'Whisper Large v3');
  assert.equal(await evaluate("speechCalls.find(call => call.command === 'speech_prepare').args.retest"), true);
  assert.ok(await evaluate("document.querySelector('.speech-summary').textContent.includes('GPU testing failed')"));
  await click('AI providers');
  await waitFor("!!document.querySelector('input[type=password]')");
  await click('Transcription');
  await waitFor("document.querySelector('.speech-settings select')?.value === 'largeV3'");
  await evaluate("speechMode = 'cancel'");
  await click('Download and test');
  await waitFor("document.querySelector('.speech-settings').textContent.includes('Testing Whisper Large v3')");
  await click('Cancel preparation');
  await waitFor("document.querySelector('[role=alert]')?.textContent.includes('Preparation cancelled')");
  await click('Refresh hardware');
  await waitFor("document.querySelector('.speech-settings').textContent.includes('Hardware checked')");
  assert.ok(await evaluate("speechCalls.some(call => call.command === 'speech_status' && call.args?.recheck === true)"));
  if (process.env.SPEECH_SCREENSHOT) {
    const { data } = await browser.send('Page.captureScreenshot', { format: 'png' });
    await writeFile(process.env.SPEECH_SCREENSHOT, Buffer.from(data, 'base64'));
  }
  console.log('Speech UI passed: fixed model, saved preference, CPU fallback display, benchmark, tabs, cancellation and hardware refresh.');
} finally {
  if (scriptId) await browser.send('Page.removeScriptToEvaluateOnNewDocument', { identifier: scriptId });
  browser.close();
}
