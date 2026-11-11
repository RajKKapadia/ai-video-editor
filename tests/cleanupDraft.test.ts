import { describe, expect, test } from 'bun:test';
import { remainingCleanupReview, restoreCleanupDraft, type CleanupReview, type SavedCleanupDraft } from '../src/cleanupDraft';
import { makeCleanupEdit } from '../src/cleanupTimeline';
import type { Segment } from '../src/types';

const timeline: Segment[] = [{ id: 'take', clipId: 'speech', in: 10, out: 30 }];
const review: CleanupReview = {
  snapshot: timeline,
  assistant: 'draft',
  report: {
    draftSummary: 'Remove a pause and a repeated take.',
    failedSources: 0, retakeIssue: null, transcriptCacheIssue: null,
    proposals: [
      { kind: 'pause', remove: { start: 2, end: 3 }, keep: null, removedText: null, keptText: null },
      { kind: 'retake', remove: { start: 8, end: 10 }, keep: { start: 12, end: 14 }, removedText: 'Earlier take', keptText: 'Complete take' },
    ],
  },
};
const saved: SavedCleanupDraft = { version: 1, review, checked: [1], pendingCuts: null };
const roundtrip = <T,>(value: T): T => JSON.parse(JSON.stringify(value));

describe('saved cleanup drafts', () => {
  test('reopens planning passes and rejection reasons with their original timeline identity', () => {
    const diagnosticReview: CleanupReview = { ...review, report: { ...review.report, planning: {
      strategy: 'fullTimeline', timelineKey: 'analyzed-timeline-hash', totalUtterances: 159, reviewedUtterances: 159,
      contextBudgetTokens: 32768, outputBudgetTokens: 10922, fallbackReason: null,
      reviews: [{ scope: 'fullTimeline', start: 0, end: 20, utterances: 159, completed: true, error: null, edits: 2,
        passes: [{ inputBytes: 18000, estimatedInputTokens: 7000, response: { cuts: [] }, validation: 'Validated 2 edits.' }] }],
    } } };
    const restored = restoreCleanupDraft(roundtrip({ ...saved, review: diagnosticReview }), roundtrip(timeline))!;
    expect(restored.review?.report.planning).toEqual(diagnosticReview.report.planning);
    const edit = makeCleanupEdit(timeline, [diagnosticReview.report.proposals[0].remove])!;
    expect(remainingCleanupReview(diagnosticReview, new Set([0]), edit)?.report.planning?.timelineKey).toBe('analyzed-timeline-hash');
    for (const planning of [null, {}, { ...diagnosticReview.report.planning, reviews: [null] },
      { ...diagnosticReview.report.planning, reviewedUtterances: 160 },
      { ...diagnosticReview.report.planning, reviews: [{ passes: 'invalid' }] },
    ]) expect(restoreCleanupDraft(roundtrip({ ...saved, review: { ...review, report: { ...review.report, planning } } }), timeline)).toBeNull();
  });
  test('restores reports and checkbox choices from JSON, binding to the reopened timeline', () => {
    const reopened = roundtrip(timeline);
    const restored = restoreCleanupDraft(roundtrip(saved), reopened)!;
    expect(restored.review?.snapshot).toBe(reopened);
    expect(restored.review?.report).toEqual(review.report);
    expect(restored.checked).toEqual([1]);
    expect(restored.pendingCuts).toBeNull();
  });

  test('restores legacy cleanup and the pending Advanced step without requiring an assistant draft', () => {
    const legacy = restoreCleanupDraft({ ...roundtrip(saved), review: { ...roundtrip(review), assistant: undefined } }, timeline)!;
    expect(legacy.review?.assistant).toBeUndefined();
    const pending = restoreCleanupDraft({ version: 1, review: null, checked: [], pendingCuts: roundtrip(timeline) }, timeline)!;
    expect(pending.pendingCuts).toBe(timeline);
  });

  test('ignores missing, malformed, future-version, or stale drafts without damaging the project', () => {
    for (const value of [undefined, {}, { ...saved, version: 2 }, { ...saved, checked: null },
      { ...saved, review: { ...review, report: { ...review.report, proposals: [null] } } },
      { ...saved, review: { ...review, report: { ...review.report, reviewNotes: 'broken' } } },
      { ...saved, review: { ...review, assistant: 'unknown' } },
    ]) expect(restoreCleanupDraft(value, timeline)).toBeNull();
    for (const changed of [
      [{ ...timeline[0], in: 11 }], [{ ...timeline[0], clipId: 'replaced' }],
      [{ ...timeline[0], id: 'another-occurrence' }], [],
    ]) expect(restoreCleanupDraft(roundtrip(saved), changed)).toBeNull();
  });

  test('filters invalid selection indices and rejects unsafe stored removal boundaries', () => {
    expect(restoreCleanupDraft({ ...saved, checked: [-1, 0, 0, 1, 2, 1.5, '1'] }, timeline)?.checked).toEqual([0, 1]);
    const broken = roundtrip(saved);
    broken.review!.report.proposals[0].remove = { start: 0.01, end: 2 };
    expect(restoreCleanupDraft(broken, timeline)).toBeNull();
  });

  test('applying pauses keeps retakes with updated removal and audition times', () => {
    const edit = makeCleanupEdit(timeline, [review.report.proposals[0].remove])!;
    const remaining = remainingCleanupReview(review, new Set([0]), edit)!;
    expect(remaining.snapshot).toBe(edit.timeline);
    expect(remaining.assistant).toBe('draft');
    expect(remaining.report.proposals).toEqual([{
      ...review.report.proposals[1], remove: { start: 7, end: 9 }, keep: { start: 11, end: 13 },
    }]);
    const restored = restoreCleanupDraft(roundtrip({ ...saved, review: remaining, checked: [] }), roundtrip(edit.timeline))!;
    expect(restored.review?.report.proposals.length).toBe(1);
    const second = makeCleanupEdit(edit.timeline, remaining.report.proposals.map(({ remove }) => remove))!;
    expect(edit.secondsRemoved + second.secondsRemoved).toBe(3);
    expect(remainingCleanupReview(remaining, new Set([0]), second)).toBeNull();
  });

  test('drops suggestions already removed by an overlapping applied cut', () => {
    const overlapping: CleanupReview = { ...review, report: { ...review.report, proposals: [
      review.report.proposals[1], { ...review.report.proposals[0], remove: { start: 8.2, end: 9 } },
    ] } };
    const edit = makeCleanupEdit(timeline, [overlapping.report.proposals[0].remove])!;
    expect(remainingCleanupReview(overlapping, new Set([0]), edit)).toBeNull();
    expect(remainingCleanupReview(review, new Set([0, 1]), makeCleanupEdit(timeline, review.report.proposals.map(({ remove }) => remove))!)).toBeNull();
  });

  test('restores fragment connections and audio checks but invalidates checks after partial application', () => {
    const verified = roundtrip(saved);
    verified.review!.report.proposals[1].kind = 'splice';
    verified.review!.report.verification = { status: 'checked', checkedPreviews: 3, rejectedEdits: 0, unresolvedEdits: 0 };
    const restored = restoreCleanupDraft(verified, timeline)!;
    expect(restored.review!.report.verification?.checkedPreviews).toBe(3);
    const edit = makeCleanupEdit(timeline, [review.report.proposals[0].remove])!;
    const remaining = remainingCleanupReview(restored.review!, new Set([0]), edit)!;
    expect(remaining.report.proposals[0].kind).toBe('splice');
    expect(remaining.report.verification).toBeUndefined();
    expect(remaining.report.reviewNotes?.join(' ')).toContain('earlier timeline');
    verified.review!.report.verification.checkedPreviews = -1;
    expect(restoreCleanupDraft(verified, timeline)).toBeNull();
  });
});
