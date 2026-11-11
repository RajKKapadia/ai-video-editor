import { segmentLength, sequenceLength, uid, type Segment } from './types';
import type { SpeechSelection } from './speechNative';

export const minSegmentDuration = 1 / 24;
const epsilon = 0.000001;

export type TimeRange = { start: number; end: number };
export type CleanupProposal = {
  kind: 'pause' | 'retake' | 'cut' | 'splice';
  remove: TimeRange;
  keep: TimeRange | null;
  removedText: string | null;
  keptText: string | null;
  reason?: string;
};
export type AssistantMessage = { id: number; role: 'user' | 'assistant'; text: string };
export type CleanupReport = {
  proposals: CleanupProposal[];
  retakeIssue: string | null;
  transcriptCacheIssue: string | null;
  failedSources: number;
  speech?: SpeechSelection | null;
  runtimeNotice?: string | null;
  assistantTurn?: AssistantMessage[];
  draftSummary?: string;
  reviewNotes?: string[];
  canClearTimeline?: boolean;
  verification?: {
    status: 'checked' | 'partial';
    checkedPreviews: number;
    rejectedEdits: number;
    unresolvedEdits: number;
  };
};

export function canPreviewCleanupDraft(timeline: Segment[], report: CleanupReport) {
  if (!report.draftSummary) return false;
  const removals = report.proposals.map(({ remove }) => remove);
  return !removals.length || (
      removals.every((remove) => makeCleanupEdit(timeline, [remove], report.canClearTimeline) !== null) &&
      makeCleanupEdit(timeline, removals, report.canClearTimeline) !== null
  );
}

export function prepareCleanupReview(timeline: Segment[], report: CleanupReport): CleanupReport {
  const notes = [...report.reviewNotes ?? []];
  let proposals = report.proposals.flatMap((proposal) => {
    if (makeCleanupEdit(timeline, [proposal.remove], report.canClearTimeline)) return [proposal];
    if (proposal.kind === 'pause') {
      const remove = fitPauseToClipEdges(timeline, proposal.remove);
      if (makeCleanupEdit(timeline, [remove], report.canClearTimeline)) return [{ ...proposal, remove }];
    }
    notes.push(`The proposed removal at ${proposal.remove.start.toFixed(2)}–${proposal.remove.end.toFixed(2)} s was left unchanged because it would leave a fragment shorter than one frame.`);
    return [];
  });
  if (proposals.length && !makeCleanupEdit(timeline, proposals.map(({ remove }) => remove), report.canClearTimeline)) {
    const conflicting = new Set<number>();
    proposals.forEach((left, index) => proposals.slice(index + 1).forEach((right, offset) => {
      if (!makeCleanupEdit(timeline, [left.remove, right.remove], report.canClearTimeline)) {
        conflicting.add(index);
        conflicting.add(index + offset + 1);
      }
    }));
    proposals = proposals.filter((_, index) => !conflicting.has(index));
    // A larger combination can also remove every retained frame. Preserve edits
    // already accepted in time order and explain any edit that cannot be added.
    const retained: CleanupProposal[] = [];
    for (const proposal of proposals) {
      if (makeCleanupEdit(timeline, [...retained, proposal].map(({ remove }) => remove), report.canClearTimeline)) retained.push(proposal);
    }
    proposals = retained;
    notes.push('Some proposed cuts were left unchanged because applying them together would leave an empty sequence or a fragment shorter than one frame.');
  }
  if (report.verification && proposals.length !== report.proposals.length) {
    const { verification: _verification, ...unverifiedReport } = report;
    notes.push('Some edits changed during timeline validation. Preview this selection or analyze again to repeat the audio checks.');
    return { ...unverifiedReport, proposals, reviewNotes: notes };
  }
  return { ...report, proposals, reviewNotes: notes };
}

function fitPauseToClipEdges(timeline: Segment[], range: TimeRange): TimeRange {
  let { start, end } = range;
  let clipStart = 0;
  for (const segment of timeline) {
    const clipEnd = clipStart + segmentLength(segment);
    if (start > clipStart + epsilon && start < clipEnd - epsilon) {
      start = Math.max(start, Math.min(clipStart + minSegmentDuration, clipEnd));
    }
    if (end > clipStart + epsilon && end < clipEnd - epsilon) {
      end = Math.min(end, Math.max(clipEnd - minSegmentDuration, clipStart));
      break;
    }
    clipStart = clipEnd;
  }
  // Move inward so the cut remains inside the measured quiet interval.
  return { start, end };
}

export function makeCleanupEdit(timeline: Segment[], ranges: TimeRange[], allowEmpty = false) {
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

  if (!next.length && !allowEmpty) return null;
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
