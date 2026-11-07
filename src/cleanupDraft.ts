import { makeCleanupEdit, prepareCleanupReview, type CleanupProposal, type CleanupReport } from './cleanupTimeline';
import { whisperModels } from './speechNative';
import type { Segment } from './types';

export type CleanupReview = {
  snapshot: Segment[];
  report: CleanupReport;
  assistant?: 'draft' | 'pauses' | 'cuts';
};

export type SavedCleanupDraft = {
  version: 1;
  review: CleanupReview | null;
  checked: number[];
  pendingCuts: Segment[] | null;
};

function sameTimeline(value: unknown, timeline: Segment[]): value is Segment[] {
  return Array.isArray(value) && value.length === timeline.length && value.every((segment, index) => {
    const current = timeline[index];
    return segment && segment.id === current.id && segment.clipId === current.clipId &&
      segment.in === current.in && segment.out === current.out;
  });
}

function validRange(value: unknown): boolean {
  if (!value || typeof value !== 'object') return false;
  const range = value as { start: number; end: number };
  return Number.isFinite(range.start) && Number.isFinite(range.end) && range.start >= 0 && range.end > range.start;
}

function validReport(value: unknown): value is CleanupReport {
  if (!value || typeof value !== 'object') return false;
  const report = value as CleanupReport;
  const optionalText = (text: unknown) => text == null || typeof text === 'string';
  return Array.isArray(report.proposals) && report.proposals.every((proposal) => proposal &&
    ['pause', 'retake', 'cut'].includes(proposal.kind) && validRange(proposal.remove) &&
    (proposal.keep == null || validRange(proposal.keep)) && optionalText(proposal.reason) &&
    optionalText(proposal.removedText) && optionalText(proposal.keptText)) &&
    optionalText(report.retakeIssue) && optionalText(report.transcriptCacheIssue) &&
    optionalText(report.runtimeNotice) && optionalText(report.draftSummary) &&
    (report.speech == null || (typeof report.speech.model === 'string' && Object.prototype.hasOwnProperty.call(whisperModels, report.speech.model) &&
      (report.speech.device == null || typeof report.speech.device.name === 'string'))) &&
    (report.reviewNotes === undefined || (Array.isArray(report.reviewNotes) && report.reviewNotes.every((note) => typeof note === 'string'))) &&
    (report.canClearTimeline === undefined || typeof report.canClearTimeline === 'boolean') &&
    Number.isInteger(report.failedSources) && report.failedSources >= 0;
}

// JSON restores separate arrays for the saved snapshot and the project timeline.
// Verify their contents before rebinding the review to the live timeline reference.
export function restoreCleanupDraft(value: unknown, timeline: Segment[]): SavedCleanupDraft | null {
  if (!value || typeof value !== 'object') return null;
  const saved = value as SavedCleanupDraft;
  if (saved.version !== 1 || !Array.isArray(saved.checked)) return null;
  const review = saved.review;
  if (review && (!sameTimeline(review.snapshot, timeline) || !validReport(review.report) ||
    (review.assistant !== undefined && !['draft', 'pauses', 'cuts'].includes(review.assistant)))) return null;
  if (saved.pendingCuts != null && !sameTimeline(saved.pendingCuts, timeline)) return null;
  if (!review && !saved.pendingCuts) return null;
  if (review && review.report.proposals.some(({ remove }) => !makeCleanupEdit(timeline, [remove], review.report.canClearTimeline))) return null;
  return {
    version: 1,
    review: review ? { ...review, snapshot: timeline } : null,
    checked: [...new Set(saved.checked.filter((index) => Number.isInteger(index) && index >= 0 && index < (review?.report.proposals.length ?? 0)))],
    pendingCuts: saved.pendingCuts ? timeline : null,
  };
}

export function remainingCleanupReview(
  review: CleanupReview,
  checked: ReadonlySet<number>,
  edit: NonNullable<ReturnType<typeof makeCleanupEdit>>,
): CleanupReview | null {
  const remap = (range: { start: number; end: number }) => ({ start: edit.mapTime(range.start), end: edit.mapTime(range.end) });
  const proposals: CleanupProposal[] = review.report.proposals.flatMap((proposal, index) => {
    if (checked.has(index)) return [];
    const remove = remap(proposal.remove);
    if (remove.end - remove.start < 0.000001) return [];
    const keep = proposal.keep ? remap(proposal.keep) : null;
    if (keep && keep.end - keep.start < 0.000001) return [];
    return [{ ...proposal, remove, keep }];
  });
  if (!proposals.length) return null;
  const report = prepareCleanupReview(edit.timeline, {
    ...review.report, proposals,
    ...(review.report.draftSummary && { draftSummary: 'Remaining suggestions from your cleanup draft. Review and apply the cuts you choose.' }),
  });
  return report.proposals.length ? { ...review, snapshot: edit.timeline, report } : null;
}
