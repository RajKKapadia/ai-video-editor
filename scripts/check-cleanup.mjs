import assert from 'node:assert/strict';
import { connect, until } from './browser-session.mjs';

const browser = await connect(Number(process.env.CHROME_PORT || 9333));
const url = process.env.CUTLINE_URL || 'http://127.0.0.1:1420/';
const evaluate = browser.evaluate;
const waitFor = (expression) => until(() => evaluate(expression));
async function click(text) {
  const button = "Array.from(document.querySelectorAll('button')).find(button => button.textContent.trim() === " +
    JSON.stringify(text) + ')';
  await waitFor('!!(' + button + ') && !(' + button + ').disabled');
  await evaluate(button + '.click()');
}
const savedTimeline = () => evaluate("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline");
const openCleanup = async () => {
  await click('Clean up timeline');
  await waitFor("document.querySelectorAll('.cleanup-options input').length === 2");
};
let scriptId;

try {
  await browser.send('Page.enable');
  ({ identifier: scriptId } = await browser.send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      window.isTauri = true;
      window.cleanupCalls = [];
      window.cleanupMock = { mode: "success" };
      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
      window.__TAURI_INTERNALS__ = {
        metadata: { currentWebview: { label: "main" }, currentWindow: { label: "main" } },
        transformCallback() { return 1; },
        async invoke(command, args) {
          cleanupCalls.push(command);
          if (command === "plugin:event|listen") return 1;
          if (command === "plugin:event|unlisten") return null;
          if (command === "load_project_cache") return JSON.parse(sessionStorage.getItem("mockProjectCache"));
          if (command === "project_cache_exists") return sessionStorage.getItem("mockProjectCache") !== null;
          if (command === "save_project_cache") {
            const saved = JSON.parse(sessionStorage.getItem("mockProjectCache"));
            if ((saved?.revision ?? null) !== args.previous) return false;
            sessionStorage.setItem("mockProjectCache", JSON.stringify(args.snapshot));
            return true;
          }
          if (command === "ai_provider_status") {
            return { active: "openAi", warning: null, profiles: {
              openAi: { model: "test-model", baseUrl: null, hasKey: true, ready: true }
            } };
          }
          if (command === "cleanup_begin") {
            if (args.projectId !== "cleanup-check") throw new Error("Missing cleanup project ID");
            return ["speech"];
          }
          if (["cleanup_source_begin", "cleanup_source_push", "cleanup_source_finish", "cleanup_cancel"].includes(command)) return null;
          if (command === "cleanup_finish") {
            if (cleanupMock.mode === "stale") await new Promise(resolve => setTimeout(resolve, 500));
            return {
              failedSources: 0,
              transcriptCacheIssue: null,
              retakeIssue: cleanupMock.mode === "partial" ? "The AI provider timed out." : null,
              proposals: [
                { kind: "pause", remove: { start: .55, end: 1.3 }, keep: null, keptText: null, removedText: null },
                ...(cleanupMock.mode === "success" ? [
                  { kind: "retake", remove: { start: 2.4, end: 3.2 }, keep: { start: .05, end: .30 }, keptText: "Keep this line", removedText: "An earlier attempt" }
                ] : []),
              ]
            };
          }
          throw new Error("Unexpected mock command " + command + " " + String(args));
        }
      };
    `,
  }));
  await browser.send('Page.navigate', { url });
  await waitFor("!!document.querySelector('.home-shell, .editor-shell')");
  await evaluate("(async () => {" +
    "const { storeMedia } = await import('/src/cache.ts');" +
    "const file = await (await fetch('/tests/fixtures/video-with-audio.mp4')).blob();" +
    "const now = Date.now();" +
    "const clips = [{id:'speech',name:'Speech fixture.mp4',kind:'video',duration:2,size:file.size,addedAt:now,hasAudio:true}];" +
    "await storeMedia('speech', file);" +
    "const timeline = [{id:'take-a',clipId:'speech',in:0,out:2},{id:'take-b',clipId:'speech',in:0,out:2}];" +
    "sessionStorage.removeItem('mockProjectCache');" +
    "localStorage.setItem('cutline.projects.v1',JSON.stringify([{id:'cleanup-check',name:'Cleanup checks',description:'',createdAt:now,updatedAt:now,clips,timeline,messages:[]}]));" +
    "location.hash='project=cleanup-check';" +
    "})()");
  await browser.send('Page.reload');
  await waitFor("!!document.querySelector('.cleanup-toggle') && !!sessionStorage.getItem('mockProjectCache') && !localStorage.getItem('cutline.projects.v1')");
  const before = await savedTimeline();
  await openCleanup();
  await evaluate("document.querySelectorAll('.cleanup-options input')[1].click()");
  await waitFor("document.querySelector('.cleanup-panel').textContent.includes('test-model')");
  await click('Analyze timeline');
  await waitFor("document.querySelectorAll('.cleanup-results li').length === 2");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 0);
  assert.deepEqual(await savedTimeline(), before);

  await click('Play kept take');
  await waitFor("Number(document.querySelector('[aria-label=\"Viewer playhead\"]').value) >= .29");
  await waitFor("document.querySelector('video').paused");
  await click('Select all pauses');
  await click('Preview join');
  await waitFor("document.querySelector('.stage-label').textContent.includes('CLEANUP PREVIEW')");
  await waitFor("document.querySelector('video').currentTime >= 1.3");
  assert.deepEqual(await savedTimeline(), before);
  const previewLength = await evaluate("document.querySelector('[aria-label=\"Viewer playhead\"]').max");

  await click('Apply selected');
  await waitFor("!document.querySelector('.cleanup-panel')");
  assert.equal(await evaluate("document.querySelector('[aria-label=\"Viewer playhead\"]').max"), previewLength);
  assert.equal((await savedTimeline()).length, 3);
  assert.deepEqual((await savedTimeline()).map((item) => [item.in, item.out]), [[0, .55], [1.3, 2], [0, 2]]);
  await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline[0].id === 'take-a'");
  assert.deepEqual(await savedTimeline(), before);

  await openCleanup();
  await evaluate("cleanupMock.mode = 'partial'; document.querySelectorAll('.cleanup-options input')[1].click()");
  await waitFor("document.querySelector('.cleanup-panel').textContent.includes('test-model')");
  await click('Analyze timeline');
  await waitFor("document.querySelector('.cleanup-panel').textContent.includes('The AI provider timed out.')");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 1);
  await click('Select all pauses');
  await click('Apply selected');
  await waitFor("!document.querySelector('.cleanup-panel')");
  const after = await savedTimeline();
  await browser.send('Page.reload');
  await waitFor("!!document.querySelector('.cleanup-toggle')");
  assert.deepEqual(await savedTimeline(), after);

  await openCleanup();
  await evaluate("cleanupMock.mode = 'stale'");
  await click('Analyze timeline');
  await waitFor("cleanupCalls.includes('cleanup_finish')");
  await evaluate("document.querySelector('.timeline-segment.video').click()");
  await waitFor("!document.querySelector('[aria-label=\"Remove selected segment\"]').disabled");
  await evaluate("document.querySelector('[aria-label=\"Remove selected segment\"]').click()");
  await waitFor("document.querySelector('.cleanup-panel').textContent.includes('The timeline changed.')");
  await waitFor("cleanupCalls.includes('cleanup_cancel') && !document.querySelector('.cleanup-progress')");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 0);
  console.log('Cleanup review, audition, join preview, apply, undo, reopening, provider fallback, and stale analysis passed.');
} finally {
  if (scriptId) await browser.send('Page.removeScriptToEvaluateOnNewDocument', { identifier: scriptId });
  browser.close();
}
