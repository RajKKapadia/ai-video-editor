import { describe, expect, test } from 'bun:test';
import { canPreviewCleanupDraft, makeCleanupEdit, prepareCleanupReview, type CleanupReport } from '../src/cleanupTimeline';
import { sequenceLength, type Segment } from '../src/types';

const original: Segment[] = [
  { id: 'first', clipId: 'shared', in: 12, out: 16 },
  { id: 'other', clipId: 'other', in: 5, out: 8 },
  { id: 'repeat', clipId: 'shared', in: 12, out: 16 },
];

describe('cleanup timeline', () => {
  test('invalidates full-draft audio checks when frame validation changes the assembly', () => {
    const report: CleanupReport = {
      proposals: [{ kind: 'splice', remove: { start: .01, end: 1 }, keep: null, removedText: null, keptText: null }],
      verification: { status: 'checked', checkedPreviews: 1, rejectedEdits: 0, unresolvedEdits: 0 },
      failedSources: 0, retakeIssue: null, transcriptCacheIssue: null,
    };
    const reviewed = prepareCleanupReview(original, report);
    expect(reviewed.proposals).toHaveLength(0);
    expect(reviewed.verification).toBeUndefined();
  });
  test('ripple edits timeline occurrences independently and preserves source offsets', () => {
    const edit = makeCleanupEdit(original, [{ start: 8, end: 9 }])!;
    expect(edit.timeline.slice(0, 2)).toEqual(original.slice(0, 2));
    expect(edit.timeline.slice(2).map(({ clipId, in: start, out }) => [clipId, start, out])).toEqual([
      ['shared', 12, 13], ['shared', 14, 16],
    ]);
    expect(new Set(edit.timeline.map((segment) => segment.id)).size).toBe(4);
    expect(edit.mapTime(9)).toBe(8);
    expect(edit.mapTime(11)).toBe(10);
    expect(edit.secondsRemoved).toBe(1);
    expect(sequenceLength(original)).toBe(11);
  });

  test('combines overlapping cuts across a clip boundary without double counting', () => {
    const edit = makeCleanupEdit(original, [{ start: 4, end: 7 }, { start: 2, end: 5 }])!;
    expect(edit.timeline.map(({ clipId, in: start, out }) => [clipId, start, out])).toEqual([
      ['shared', 12, 14], ['shared', 12, 16],
    ]);
    expect(edit.secondsRemoved).toBe(5);
    expect(edit.mapTime(4)).toBe(2);
  });

  test('rejects cuts that leave a new fragment shorter than one frame or delete the sequence', () => {
    expect(makeCleanupEdit(original, [{ start: 0.01, end: 1 }])).toBeNull();
    expect(makeCleanupEdit(original, [{ start: 0, end: 11 }])).toBeNull();
    expect(makeCleanupEdit(original, [{ start: 2, end: 12 }])).toBeNull();
    expect(makeCleanupEdit(original, [{ start: NaN, end: 2 }])).toBeNull();
  });

  test('can clear a sequence only when the backend verified that all source audio is quiet', () => {
    expect(makeCleanupEdit(original, [{ start: 0, end: 11 }])).toBeNull();
    const cleared = makeCleanupEdit(original, [{ start: 0, end: 11 }], true)!;
    expect(cleared.timeline).toEqual([]);
    expect(cleared.secondsRemoved).toBe(11);
    expect(cleared.mapTime(7)).toBe(0);
  });

  test('rejects a draft that only creates a sub-frame fragment when cuts are combined', () => {
    const report: CleanupReport = {
      draftSummary: 'Draft', failedSources: 0, retakeIssue: null, transcriptCacheIssue: null,
      proposals: [{ start: 1, end: 2 }, { start: 2.01, end: 3 }].map((remove) => ({
        kind: 'pause', remove, keep: null, keptText: null, removedText: null,
      })),
    };
    expect(canPreviewCleanupDraft(original, { ...report, proposals: report.proposals.slice(0, 1) })).toBe(true);
    expect(canPreviewCleanupDraft(original, report)).toBe(false);
    const reviewed = prepareCleanupReview(original, report);
    expect(reviewed.proposals).toEqual([]);
    expect(reviewed.reviewNotes?.length).toBe(1);
    expect(canPreviewCleanupDraft(original, reviewed)).toBe(true);
  });

  test('retains independent valid cuts and explains invalid frame boundaries', () => {
    const report: CleanupReport = {
      draftSummary: 'Draft', failedSources: 1, retakeIssue: 'Provider unavailable', transcriptCacheIssue: null,
      proposals: [{ start: 0.01, end: 1 }, { start: 2, end: 3 }].map((remove) => ({
        kind: 'cut', remove, keep: null, keptText: null, removedText: null,
      })),
    };
    const reviewed = prepareCleanupReview(original, report);
    expect(reviewed.proposals.map(({ remove }) => remove)).toEqual([{ start: 2, end: 3 }]);
    expect(reviewed.reviewNotes?.length).toBe(1);
    expect(canPreviewCleanupDraft(original, reviewed)).toBe(true);
  });

  test('keeps a full frame by shortening a measured silence removal at clip edges', () => {
    const report: CleanupReport = {
      draftSummary: 'Draft', failedSources: 0, retakeIssue: null, transcriptCacheIssue: null,
      proposals: [{ kind: 'pause', remove: { start: 0.02, end: 3.97 }, keep: null, keptText: null, removedText: null }],
    };
    const reviewed = prepareCleanupReview(original, report);
    expect(reviewed.proposals).toHaveLength(1);
    const removal = reviewed.proposals[0].remove;
    expect(removal.start).toBeCloseTo(1 / 24, 8);
    expect(removal.end).toBeCloseTo(4 - 1 / 24, 8);
    expect(makeCleanupEdit(original, [removal])?.secondsRemoved).toBeCloseTo(4 - 2 / 24, 8);
  });
});
