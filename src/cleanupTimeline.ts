import { segmentLength, sequenceLength, uid, type Segment } from './types';
import type { SpeechSelection } from './speechNative';

export const minSegmentDuration = 1 / 24;
const epsilon = 0.000001;

export type TimeRange = { start: number; end: number };
export type CleanupProposal = {
  kind: 'pause' | 'retake' | 'cut';
  remove: TimeRange;
  keep: TimeRange | null;
  removedText: string | null;
  keptText: string | null;
  reason?: string;
};
export type AssistantMessage = { id: number; role: 'user' | 'assistant'; text: string };
export type PlanningDiagnostics = {
  strategy: 'fullTimeline' | 'sections';
  provider?: string;
  model?: string;
  timelineKey: string;
  totalUtterances: number;
  reviewedUtterances: number;
  contextBudgetTokens: number;
  outputBudgetTokens: number;
  fallbackReason: string | null;
  reviews: {
    scope: string; start: number; end: number; utterances: number;
    completed: boolean; error: string | null; edits: number;
    passes: { inputBytes: number; estimatedInputTokens: number; response: unknown; validation: string }[];
  }[];
};
export type CleanupReport = {
  processingDevice?: string | null;
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
  planning?: PlanningDiagnostics;
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
  let proposals = report.proposals.filter((proposal) => {
    if (makeCleanupEdit(timeline, [proposal.remove], report.canClearTimeline)) return true;
    notes.push(`The proposed removal at ${proposal.remove.start.toFixed(2)}–${proposal.remove.end.toFixed(2)} s was left unchanged because it would leave a fragment shorter than one frame.`);
    return false;
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
  return { ...report, proposals, reviewNotes: notes };
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
