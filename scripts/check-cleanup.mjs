import assert from 'node:assert/strict';
import { connect, until } from './browser-session.mjs';

const browser = await connect(Number(process.env.CHROME_PORT || 9333));
const url = process.env.CUTLINE_URL || 'http://127.0.0.1:1420/';
const evaluate = browser.evaluate;
const waitFor = (expression) => until(async () => {
  try {
    return await evaluate(expression);
  } catch (error) {
    if (!/Inspected target navigated|Cannot find context/.test(error.message)) throw error;
    return false;
  }
});
async function click(text) {
  const button = "Array.from(document.querySelectorAll('button')).find(button => !button.disabled && button.textContent.trim() === " +
    JSON.stringify(text) + ')';
  await waitFor('(() => { const button = ' + button + '; if (!button) return false; button.click(); return true; })()');
}
const savedTimeline = () => evaluate("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline");
const savedDraft = () => evaluate("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].cleanupDraft");
const openCleanup = async () => {
  if (!await evaluate("!!document.querySelector('.cleanup-options')")) await click('Clean up timeline');
  await waitFor("document.querySelectorAll('.cleanup-options input').length === 2");
};
async function reloadReview(panel, count, checked) {
  await evaluate('window.reloadingCleanupCheck = true');
  await browser.send('Page.reload');
  await waitFor("!window.reloadingCleanupCheck && document.querySelector(" + JSON.stringify(panel) + ") && document.querySelectorAll('.cleanup-results li').length === " + count);
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), checked);
  await waitFor("!!document.querySelector('.stage-label')");
  assert.equal(await evaluate("document.querySelector('.stage-label').textContent.includes('CLEANUP PREVIEW')"), false);
  assert.equal(await evaluate("cleanupCalls.includes('cleanup_begin')"), false);
}
let scriptId;

try {
  await browser.send('Page.enable');
  ({ identifier: scriptId } = await browser.send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      window.isTauri = true;
      window.cleanupCalls = [];
      window.assistantRequests = [];
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
          if (command === "assistant_history") return [];
          if (command === "cleanup_begin") {
            if (args.projectId !== "cleanup-check") throw new Error("Missing cleanup project ID");
            cleanupMock.stage = args.assistant?.stage;
            if (args.assistant) assistantRequests.push(args.assistant);
            return ["speech"];
          }
          if (["cleanup_source_begin", "cleanup_source_push", "cleanup_source_finish", "cleanup_cancel"].includes(command)) return null;
          if (command === "cleanup_finish") {
            const { mode, stage } = cleanupMock;
            if (mode === "stale") await new Promise(resolve => setTimeout(resolve, 500));
            const pause = { kind: "pause", remove: { start: .55, end: 1.3 }, keep: null, keptText: null, removedText: null };
            const retake = { kind: "retake", remove: { start: 2.4, end: 3.2 }, keep: { start: .05, end: .30 }, keptText: "Keep this line", removedText: "An earlier attempt" };
            if (stage === "draft" && mode === "unavailable") throw new Error("Some timeline sources could not be analyzed.");
            return {
              failedSources: 0,
              transcriptCacheIssue: null,
              retakeIssue: mode === "partial" ? "The AI provider timed out." : null,
              ...(stage === "draft" && { draftSummary: "Review the proposed edits." }),
              ...(stage === "draft" && { planning: {
                strategy: "fullTimeline", timelineKey: "mock-analyzed-timeline", totalUtterances: 159, reviewedUtterances: 159,
                contextBudgetTokens: 32768, outputBudgetTokens: 10922, fallbackReason: null,
                reviews: [{ scope: "fullTimeline", start: 0, end: 4, utterances: 159, completed: true, error: null, edits: 2,
                  passes: [{ inputBytes: 18000, estimatedInputTokens: 7000, response: { cuts: [] }, validation: "Validated 2 edits." }] }]
              } }),
              ...(stage === "draft" && mode === "quiet" && { canClearTimeline: true }),
              ...(stage === "draft" && mode === "splice" && { verification: { status: "checked", checkedPreviews: 2, rejectedEdits: 0, unresolvedEdits: 0 } }),
              ...(stage === "chat" && { assistantTurn: [{ id: 1, role: "assistant", text: "Transcript answer" }] }),
              proposals: stage === "draft" && mode === "quiet" ? [{ ...pause, remove: { start: 0, end: 4 } }] : stage === "draft" ? (
                mode === "splice" ? [{ ...retake, kind: "splice", reason: "Connect the partial attempt to its continuation" }] :
                mode === "invalid" ? [{ ...pause, kind: "cut", remove: { start: .01, end: 1.3 } }] :
                mode === "empty" ? [] : [pause, retake]
              ) : stage === "chat" ? [] : stage === "cuts" ? [retake] :
                mode === "success" && !stage ? [pause, retake] : [pause],
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
  await click('Select all retakes');
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 1);
  await click('Select all pauses');
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 2);
  await click('Clear selection');
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 0);
  await click('Select all pauses');
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].cleanupDraft?.checked.length === 1");
  await reloadReview('.cleanup-options', 2, 1);
  assert.deepEqual(await savedTimeline(), before);
  await click('Preview join');
  await waitFor("document.querySelector('.stage-label').textContent.includes('CLEANUP PREVIEW')");
  await waitFor("document.querySelector('video').currentTime >= 1.3");
  assert.deepEqual(await savedTimeline(), before);
  const previewLength = await evaluate("document.querySelector('[aria-label=\"Viewer playhead\"]').max");

  await click('Apply selected');
  await waitFor("document.querySelectorAll('.cleanup-results li').length === 1");
  assert.equal(await evaluate("document.querySelector('[aria-label=\"Viewer playhead\"]').max"), previewLength);
  assert.equal((await savedTimeline()).length, 3);
  assert.deepEqual((await savedTimeline()).map((item) => [item.in, item.out]), [[0, .55], [1.3, 2], [0, 2]]);
  assert.ok(Math.abs((await savedDraft()).review.report.proposals[0].remove.start - 1.65) < .00001);
  await click('Select all retakes');
  await click('Apply selected');
  await waitFor("!document.querySelector('.cleanup-panel')");
  assert.deepEqual((await savedTimeline()).map(({ in: start, out }) => [start, out].map(time => Number(time.toFixed(3)))), [[0, .55], [1.3, 2], [0, .4], [1.2, 2]]);
  assert.equal(await savedDraft(), undefined);
  await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
  await openCleanup();
  await waitFor("document.querySelectorAll('.cleanup-results li').length === 1");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 1);
  await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline[0].id === 'take-a'");
  assert.deepEqual(await savedTimeline(), before);
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 1);

  await openCleanup();
  await evaluate("cleanupMock.mode = 'partial'; document.querySelectorAll('.cleanup-options input')[1].click()");
  await waitFor("document.querySelector('.cleanup-panel').textContent.includes('test-model')");
  await click('Analyze again');
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
  console.log('Cleanup bulk selection, saved drafts, partial apply, audition, join preview, Undo, provider fallback, and stale analysis passed.');

  // Restore the original two takes in the disposable profile.
  await evaluate("(() => {const snapshot = JSON.parse(sessionStorage.getItem('mockProjectCache'));" +
    "snapshot.projects[0].timeline = " + JSON.stringify(before) + ";" +
    "sessionStorage.setItem('mockProjectCache', JSON.stringify(snapshot)); window.reloadingAssistantCheck = true;})()");
  await browser.send('Page.reload');
  await waitFor("!window.reloadingAssistantCheck && !!document.querySelector('.editor-shell')");
  await openCleanup();
  await click('Draft adaptive cleanup');
  await waitFor("document.querySelector('.assistant-panel')?.textContent.includes('test-model')");
  await waitFor("document.querySelectorAll('.cleanup-results li').length === 2");
  assert.equal(await evaluate("assistantRequests[0].stage"), 'draft');
  assert.deepEqual(await savedTimeline(), before);
  assert.equal(await evaluate("document.querySelector('.assistant-advanced').open"), false);

  await click('Draft cleanup');
  await waitFor("document.querySelectorAll('.cleanup-results li').length === 2");
  assert.equal(await evaluate("assistantRequests[0].stage"), 'draft');
  assert.match(await evaluate("assistantRequests[0].message"), /Preserve unique details/);
  assert.match(await evaluate("document.querySelector('.cleanup-planning-details').textContent"), /159 of 159/);
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 0);
  await click('Select all pauses');
  await click('Select all retakes');
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results input:checked').length"), 2);
  await click('Clear selection');
  await click('Select all retakes');
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].cleanupDraft?.checked[0] === 1");
  await click('Preview selected');
  await reloadReview('.assistant-panel', 2, 1);
  assert.equal((await savedDraft()).review.report.planning.timelineKey, 'mock-analyzed-timeline');
  assert.match(await evaluate("document.querySelector('.cleanup-planning-details').textContent"), /Validated 2 edits/);
  assert.deepEqual(await savedTimeline(), before);
  await evaluate("cleanupMock.mode = 'unavailable'");
  await click('Draft cleanup');
  await waitFor("document.querySelector('.assistant-panel')?.textContent.includes('Some timeline sources could not be analyzed.')");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 2);
  assert.deepEqual((await savedDraft()).checked, [1]);
  await evaluate("cleanupMock.mode = 'stale'");
  await click('Draft cleanup');
  await click('Cancel analysis');
  await waitFor("!document.querySelector('.cleanup-progress')");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 2);
  assert.deepEqual((await savedDraft()).checked, [1]);
  await evaluate("cleanupMock.mode = 'success'");
  await click('Play kept take');
  await waitFor("document.querySelector('video').currentTime >= .25 && document.querySelector('video').paused");
  await click('Preview join');
  await waitFor("document.querySelector('.stage-label').textContent.includes('CLEANUP PREVIEW')");
  assert.deepEqual(await savedTimeline(), before);
  await click('Apply selected');
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 3");
  assert.deepEqual((await savedTimeline()).map(({ in: start, out }) => [start, out].map(time => Number(time.toFixed(3)))), [[0, 2], [0, .4], [1.2, 2]]);
  await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 2");
  assert.deepEqual(await savedTimeline(), before);

  for (const [mode, message] of [
    ['invalid', 'fragment shorter than one frame'],
    ['unavailable', 'sources could not be analyzed'],
    ['empty', 'No cuts suggested'],
  ]) {
    await evaluate('cleanupMock.mode = ' + JSON.stringify(mode));
    await click('Draft cleanup');
    await waitFor("document.querySelector('.assistant-panel')?.textContent.includes(" + JSON.stringify(message) + ")");
    assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 0);
    assert.deepEqual(await savedTimeline(), before);
  }

  await evaluate("cleanupMock.mode = 'partial'");
  await click('Draft cleanup');
  await waitFor("document.querySelector('.assistant-panel')?.textContent.includes('The AI provider timed out.')");
  assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 2);
  assert.deepEqual(await savedTimeline(), before);

  await evaluate("cleanupMock.mode = 'quiet'");
  await click('Draft cleanup');
  await waitFor("document.querySelector('.assistant-panel')?.textContent.includes('clear the timeline')");
  await evaluate("document.querySelector('.cleanup-results input').click()");
  await click('Preview selected');
  assert.deepEqual(await savedTimeline(), before);
  await click('Apply selected');
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 0");
  await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 2");
  assert.deepEqual(await savedTimeline(), before);

  await evaluate("cleanupMock.mode = 'splice'");
  await click('Draft cleanup');
  await waitFor("document.querySelector('.assistant-panel')?.textContent.includes('Fragment connection')");
  assert.equal(await evaluate("document.querySelector('.assistant-panel').textContent.includes('2 local audio previews checked.')"), true);
  await click('Select all retakes');
  await reloadReview('.assistant-panel', 1, 1);
  assert.equal(await evaluate("document.querySelector('.assistant-panel').textContent.includes('Fragment connection')"), true);
  await click('Apply selected');
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 3");
  await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
  await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 2");
  assert.deepEqual(await savedTimeline(), before);

  async function removeTimelineSegment() {
    await evaluate("document.querySelector('.timeline-segment.video').click()");
    await waitFor("!document.querySelector('[aria-label=\"Remove selected segment\"]').disabled");
    await evaluate("document.querySelector('[aria-label=\"Remove selected segment\"]').click()");
    await waitFor("document.querySelector('.assistant-panel')?.textContent.includes('The timeline changed.')");
    await waitFor("!document.querySelector('.cleanup-progress')");
    assert.equal(await evaluate("document.querySelectorAll('.cleanup-results li').length"), 0);
    await evaluate("document.querySelector('[aria-label=\"Undo timeline edit\"]').click()");
    await waitFor("JSON.parse(sessionStorage.getItem('mockProjectCache')).projects[0].timeline.length === 2");
    assert.deepEqual(await savedTimeline(), before);
  }
  await evaluate("cleanupMock.mode = 'stale'; cleanupCalls.length = 0");
  await click('Draft cleanup');
  await waitFor("cleanupCalls.includes('cleanup_finish')");
  await removeTimelineSegment();
  assert.ok(await evaluate("cleanupCalls.includes('cleanup_cancel')"));
  await evaluate("cleanupMock.mode = 'success'");
  await click('Draft cleanup');
  await waitFor("document.querySelectorAll('.cleanup-results li').length === 2");
  await removeTimelineSegment();

  await evaluate("document.querySelector('.assistant-advanced summary').click()");
  await click('Analyze transcript and pauses');
  await waitFor("!!document.querySelector('[aria-label=\"Suggested pauses\"]')");
  await click('Find cuts without removing pauses');
  await waitFor("document.querySelector('[aria-label=\"Suggested cuts\"]')?.textContent.includes('An earlier attempt')");
  assert.deepEqual(await evaluate("assistantRequests.slice(-2).map(request => request.stage)"), ['pauses', 'cuts']);
  await evaluate("(() => {const prompt = document.querySelector('#timeline-assistant-prompt');" +
    "Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(prompt, 'What was said?');" +
    "prompt.dispatchEvent(new Event('input', { bubbles: true }));})()");
  await click('Ask about transcript');
  await waitFor("document.querySelector('.assistant-conversation')?.textContent.includes('Transcript answer')");
  assert.equal(await evaluate("assistantRequests.at(-1).stage"), 'chat');
  console.log('Assistant draft preview, selective apply, undo, invalid and empty results, stale drafts, Advanced, and Q&A passed.');
} finally {
  if (scriptId) await browser.send('Page.removeScriptToEvaluateOnNewDocument', { identifier: scriptId });
  browser.close();
}
