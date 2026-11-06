import { minSegmentDuration, type TimeRange } from './cleanupTimeline';
import { segmentLength, sequenceLength, uid, type Segment } from './types';

export type RangeMarks = { in?: number; out?: number };
const epsilon = 0.000001;

export function setRangeMark(previous: RangeMarks, type: 'in' | 'out', time: number): RangeMarks {
  return type === 'in'
    ? { in: time, out: previous.out !== undefined && previous.out > time ? previous.out : undefined }
    : { in: previous.in !== undefined && previous.in < time ? previous.in : undefined, out: time };
}

export function markedRange(marks: RangeMarks, duration: number): TimeRange | null {
  if (marks.in === undefined && marks.out === undefined) return null;
  const start = marks.in ?? 0;
  const end = marks.out ?? duration;
  if (!Number.isFinite(start) || !Number.isFinite(end) || start < 0 || end > duration ||
    end - start < minSegmentDuration - epsilon) return null;
  return { start, end };
}

// A contiguous ripple edit: preserve everything outside the target in source time.
// Video and its linked audio share these segments, so they receive the same edit.
export function editTimelineRange(timeline: Segment[], range: TimeRange, replacement?: Segment): Segment[] | null {
  const duration = sequenceLength(timeline);
  const { start, end } = range;
  if (!Number.isFinite(start) || !Number.isFinite(end) || start < 0 || end > duration + epsilon ||
    end - start < minSegmentDuration - epsilon) return null;
  if (replacement && (!Number.isFinite(replacement.in) || !Number.isFinite(replacement.out) ||
    replacement.in < 0 || segmentLength(replacement) < minSegmentDuration - epsilon ||
    timeline.some((segment) => segment.id === replacement.id))) return null;

  const before: Segment[] = [];
  const after: Segment[] = [];
  let cursor = 0;
  for (const segment of timeline) {
    const segmentEnd = cursor + segmentLength(segment);
    if (segmentEnd <= start + epsilon) before.push(segment);
    else if (cursor >= end - epsilon) after.push(segment);
    else {
      const left = start - cursor;
      const right = segmentEnd - end;
      // Avoid creating sub-frame fragments at either new cut boundary.
      if ((left > epsilon && left < minSegmentDuration - epsilon) ||
        (right > epsilon && right < minSegmentDuration - epsilon)) return null;
      if (left > epsilon) before.push({ ...segment, out: segment.in + left });
      if (right > epsilon) after.push({
        ...segment,
        id: left > epsilon ? uid() : segment.id,
        in: segment.out - right,
      });
    }
    cursor = segmentEnd;
  }
  return [...before, ...(replacement ? [replacement] : []), ...after];
}
