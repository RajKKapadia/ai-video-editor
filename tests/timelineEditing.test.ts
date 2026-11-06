import { describe, expect, test } from 'bun:test';
import { editTimelineRange, markedRange, setRangeMark } from '../src/timelineEditing';
import { sequenceLength, type Segment } from '../src/types';

const original: Segment[] = [
  { id: 'first', clipId: 'shared', in: 12, out: 16 },
  { id: 'middle', clipId: 'other', in: 5, out: 8 },
  { id: 'last', clipId: 'shared', in: 20, out: 24 },
];
const replacement: Segment = { id: 'replacement', clipId: 'source', in: 2, out: 4 };
const ranges = (timeline: Segment[]) => timeline.map(({ clipId, in: start, out }) => [clipId, start, out]);

describe('timeline range marks', () => {
  test('unmarked timelines have no implicit edit target', () => {
    expect(markedRange({}, 11)).toBeNull();
  });

  test('one mark extends to the opposite sequence boundary', () => {
    expect(markedRange({ in: 4 }, 11)).toEqual({ start: 4, end: 11 });
    expect(markedRange({ out: 7 }, 11)).toEqual({ start: 0, end: 7 });
  });

  test('moving a mark past the other clears the crossed mark', () => {
    expect(setRangeMark({ in: 2, out: 5 }, 'in', 7)).toEqual({ in: 7, out: undefined });
    expect(setRangeMark({ in: 7, out: 9 }, 'out', 3)).toEqual({ in: undefined, out: 3 });
    expect(setRangeMark({ in: 2, out: 9 }, 'in', 3)).toEqual({ in: 3, out: 9 });
  });

  test('empty, stale and non-finite marks cannot form an edit target', () => {
    for (const marks of [{ in: 11 }, { out: 0 }, { in: 2, out: 2.01 },
      { in: -1 }, { out: 12 }, { in: NaN }, { out: Infinity }]) {
      expect(markedRange(marks, 11)).toBeNull();
    }
  });
});

describe('timeline range edits', () => {
  test('deletes across clips while preserving trimmed source offsets and inputs', () => {
    const snapshot = structuredClone(original);
    const next = editTimelineRange(original, { start: 2, end: 8 })!;
    expect(ranges(next)).toEqual([['shared', 12, 14], ['shared', 21, 24]]);
    expect(sequenceLength(next)).toBe(5);
    expect(original).toEqual(snapshot);
  });

  test('replaces across cuts and ripples by the duration difference', () => {
    const next = editTimelineRange(original, { start: 2, end: 8 }, replacement)!;
    expect(ranges(next)).toEqual([['shared', 12, 14], ['source', 2, 4], ['shared', 21, 24]]);
    expect(sequenceLength(next)).toBe(7);
    expect(next[1]).toBe(replacement);
  });

  test('keeps both ends of one segment with independent IDs', () => {
    const next = editTimelineRange(original, { start: 1, end: 3 }, replacement)!;
    expect(ranges(next.slice(0, 3))).toEqual([['shared', 12, 13], ['source', 2, 4], ['shared', 15, 16]]);
    expect(new Set(next.map(({ id }) => id)).size).toBe(next.length);
    expect(next[0].id).toBe('first');
    expect(next.slice(3)).toEqual(original.slice(1));
  });

  test('exact cut boundaries leave surrounding segments intact', () => {
    const next = editTimelineRange(original, { start: 4, end: 7 })!;
    expect(next).toEqual([original[0], original[2]]);
    expect(next[0]).toBe(original[0]);
    expect(next[1]).toBe(original[2]);
  });

  test('supports deleting the whole timeline and replacing all media', () => {
    expect(editTimelineRange(original, { start: 0, end: 11 })).toEqual([]);
    expect(editTimelineRange(original, { start: 0, end: 11 }, replacement)).toEqual([replacement]);
  });

  test('a longer replacement moves later clips without changing their source trims', () => {
    const longer = { ...replacement, out: 10 };
    const next = editTimelineRange(original, { start: 4, end: 7 }, longer)!;
    expect(next).toEqual([original[0], longer, original[2]]);
    expect(sequenceLength(next)).toBe(16);
  });

  test('rejects sub-frame leftovers and accepts a one-frame fragment', () => {
    expect(editTimelineRange(original, { start: 0.01, end: 2 })).toBeNull();
    expect(editTimelineRange(original, { start: 2, end: 3.99 })).toBeNull();
    const next = editTimelineRange(original, { start: 1 / 24, end: 4 })!;
    expect(next[0].out - next[0].in).toBeCloseTo(1 / 24);
  });

  test('rejects invalid ranges and replacement segments without changing the timeline', () => {
    for (const range of [{ start: -1, end: 2 }, { start: 2, end: 12 },
      { start: 3, end: 2 }, { start: 2, end: 2 }, { start: NaN, end: 3 },
      { start: 2, end: Infinity }]) {
      expect(editTimelineRange(original, range, replacement)).toBeNull();
    }
    for (const bad of [{ ...replacement, out: 2 }, { ...replacement, in: -1 },
      { ...replacement, out: Infinity }, { ...replacement, id: 'first' }]) {
      expect(editTimelineRange(original, { start: 0, end: 4 }, bad)).toBeNull();
    }
    expect(editTimelineRange([], { start: 0, end: 1 }, replacement)).toBeNull();
  });
});
