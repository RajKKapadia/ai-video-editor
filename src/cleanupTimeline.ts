import { segmentLength, sequenceLength, uid, type Segment } from './types';
import type { SpeechSelection } from './speechNative';

export const minSegmentDuration = 1 / 24;
const epsilon = 0.000001;

export type TimeRange = { start: number; end: number };
export type CleanupProposal = {
  kind: 'pause' | 'retake';
  remove: TimeRange;
  keep: TimeRange | null;
  removedText: string | null;
  keptText: string | null;
};
export type CleanupReport = {
  proposals: CleanupProposal[];
  retakeIssue: string | null;
  transcriptCacheIssue: string | null;
  failedSources: number;
  speech?: SpeechSelection | null;
  runtimeNotice?: string | null;
};

export function makeCleanupEdit(timeline: Segment[], ranges: TimeRange[]) {
  if (!ranges.length) return null;
  const duration = sequenceLength(timeline);
  const cuts: TimeRange[] = [];
  for (const range of [...ranges].sort((left, right) => left.start - right.start)) {
    if (
      !Number.isFinite(range.start) ||
      !Number.isFinite(range.end) ||
      range.start < -epsilon ||
      range.end > duration + epsilon ||
      range.end - range.start <= epsilon
    ) return null;
    const cut = { start: Math.max(0, range.start), end: Math.min(duration, range.end) };
    const previous = cuts[cuts.length - 1];
    if (previous && previous.end + epsilon >= cut.start) {
      previous.end = Math.max(previous.end, cut.end);
    } else {
      cuts.push(cut);
    }
  }

  const next: Segment[] = [];
  let cutIndex = 0;
  let position = 0;
  for (const segment of timeline) {
    const end = position + segmentLength(segment);
    while (cuts[cutIndex]?.end <= position + epsilon) cutIndex++;
    if (!cuts[cutIndex] || cuts[cutIndex].start >= end - epsilon) {
      next.push(segment);
      position = end;
      continue;
    }

    const kept: TimeRange[] = [];
    let start = position;
    let index = cutIndex;
    while (cuts[index] && cuts[index].start < end - epsilon) {
      const cut = cuts[index];
      if (cut.start - start > epsilon) kept.push({ start, end: cut.start });
      start = Math.max(start, cut.end);
      if (cut.end >= end - epsilon) break;
      index++;
    }
    if (end - start > epsilon) kept.push({ start, end });
    for (const range of kept) {
      if (range.end - range.start < minSegmentDuration - epsilon) return null;
      next.push({
        id: uid(),
        clipId: segment.clipId,
        in: segment.in + range.start - position,
        out: segment.out - (end - range.end),
      });
    }
    cutIndex = index;
    position = end;
  }

  if (!next.length) return null;
  const secondsRemoved = duration - sequenceLength(next);
  if (secondsRemoved <= epsilon) return null;
  return {
    timeline: next,
    secondsRemoved,
    mapTime(time: number) {
      const removed = cuts.reduce(
        (total, cut) => total + Math.max(0, Math.min(time, cut.end) - cut.start),
        0,
      );
      return Math.max(0, Math.min(duration - secondsRemoved, time - removed));
    },
  };
}
