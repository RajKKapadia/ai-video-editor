import { describe, expect, test } from 'bun:test';
import { canPreviewCleanupDraft, makeCleanupEdit, type CleanupReport } from '../src/cleanupTimeline';
import { sequenceLength, type Segment } from '../src/types';

const original: Segment[] = [
  { id: 'first', clipId: 'shared', in: 12, out: 16 },
  { id: 'other', clipId: 'other', in: 5, out: 8 },
  { id: 'repeat', clipId: 'shared', in: 12, out: 16 },
];

describe('cleanup timeline', () => {
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

  test('rejects a draft that only creates a sub-frame fragment when cuts are combined', () => {
    const report: CleanupReport = {
      draftSummary: 'Draft', failedSources: 0, retakeIssue: null, transcriptCacheIssue: null,
      proposals: [{ start: 1, end: 2 }, { start: 2.01, end: 3 }].map((remove) => ({
        kind: 'pause', remove, keep: null, keptText: null, removedText: null,
      })),
    };
    expect(canPreviewCleanupDraft(original, { ...report, proposals: report.proposals.slice(0, 1) })).toBe(true);
    expect(canPreviewCleanupDraft(original, report)).toBe(false);
  });
});
