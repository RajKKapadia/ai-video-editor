import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from 'react';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { Brand } from './App';
import { AssistantPanel } from './AssistantPanel';
import { CleanupPanel } from './CleanupPanel';
import { remainingCleanupReview, restoreCleanupDraft, type CleanupReview, type SavedCleanupDraft } from './cleanupDraft';
import { ExportPanel } from './ExportPanel';
import { Icon, type IconName } from './Icon';
import { adaptiveCleanupBrief, analyzeTimeline, errorMessage, hasAnalyzableAudio, type AssistantRequest, type CleanupOptions, type CleanupProgress } from './cleanupNative';
import { canPreviewCleanupDraft, makeCleanupEdit, minSegmentDuration, prepareCleanupReview, type AssistantMessage, type TimeRange } from './cleanupTimeline';
import { inspectMedia, storeMedia } from './cache';
import { importConcurrency, mapConcurrent } from './resources';
import { TimelineRuler, useTimelineZoom } from './timelineView';
import { editTimelineRange, markedRange, setRangeMark, type RangeMarks } from './timelineEditing';
import { TimelineWaveforms } from './TimelineWaveforms';
import { useMediaUrls } from './useMediaUrls';
import { inspectMediaPaths, useLinkedMedia } from './useLinkedMedia';
import { useSavedAudio } from './useSavedAudio';
import { useWaveforms } from './useWaveforms';
import {
  insertSegment,
  sequenceLength,
  segmentLength,
  shortTime,
  splitSegment,
  timecode,
  uid,
  type Clip,
  type Project,
  type Segment,
} from './types';

type EditorProps = {
  project: Project;
  onUpdate: (patch: (project: Project) => Project, metadataOnly?: boolean) => void;
  onHome: () => void;
  onShortcuts: () => void;
  onSettings: (tab?: 'speech' | 'providers') => void;
  providerRevision: number;
  cacheError: string | null;
};
function Tool({
  icon,
  label,
  onClick,
  disabled,
  active,
  children,
}: {
  icon: IconName;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  active?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <button
      className={`icon-button ${active ? 'is-active' : ''}`}
      aria-label={label}
      title={label}
      onClick={onClick}
      disabled={disabled}
    >
      {children || <Icon name={icon} size={17} />}
    </button>
  );
}
function Thumbnail({ clip }: { clip: Clip }) {
  return clip.thumbnail ? (
    <img
      src={clip.thumbnail}
      alt=""
      draggable={false}
      loading="lazy"
      decoding="async"
    />
  ) : (
    <div className="audio-thumbnail">
      <Icon name={clip.kind === 'audio' ? 'music' : 'film'} size={26} />
    </div>
  );
}

export function Editor({
  project,
  onUpdate,
  onHome,
  onShortcuts,
  onSettings,
  providerRevision,
  cacheError,
}: EditorProps) {
  const desktop = isTauri();
  const [restoredDraft] = useState(() => restoreCleanupDraft(project.cleanupDraft, project.timeline));
  const [selectedId, setSelectedId] = useState<string | null>(
    project.clips[0]?.id || null,
  );
  const [mode, setMode] = useState<'source' | 'sequence'>('source');
  const [sourceTime, setSourceTime] = useState(0);
  const [sequenceTime, setSequenceTime] = useState(0);
  const [marks, setMarks] = useState<
    Record<string, { in?: number; out?: number }>
  >({});
  const [sequenceMarks, setSequenceMarks] = useState<RangeMarks>({});
  const [playing, setPlaying] = useState(false);
  const [muted, setMuted] = useState(false);
  const [filter, setFilter] = useState('all');
  const [search, setSearch] = useState('');
  const [sort, setSort] = useState('name');
  const [layout, setLayout] = useState('grid');
  const [aiOpen, setAiOpen] = useState(() => !!restoredDraft?.review?.assistant || window.innerWidth > 1000);
  const [prompt, setPrompt] = useState('');
  const [notice, setNotice] = useState('');
  const [importing, setImporting] = useState(false);
  const [dropping, setDropping] = useState(false);
  const [selectedSegment, setSelectedSegment] = useState<string | null>(null);
  const [history, setHistory] = useState<{ before: Segment[]; after: Segment[]; cleanup: SavedCleanupDraft | null }[]>([]);
  const [cleanupOpen, setCleanupOpen] = useState(() => !!restoredDraft?.review && !restoredDraft.review.assistant);
  const [exportOpen, setExportOpen] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  const [cleanupProgress, setCleanupProgress] = useState<CleanupProgress | null>(null);
  const [cleanupError, setCleanupError] = useState(() => project.cleanupDraft && !restoredDraft
    ? 'The saved cleanup draft no longer matches this timeline. Run the analysis again.' : '');
  const [cleanupReview, setCleanupReview] = useState<CleanupReview | null>(restoredDraft?.review ?? null);
  const [assistantMessages, setAssistantMessages] = useState<AssistantMessage[]>([]);
  const [pendingCuts, setPendingCuts] = useState<Segment[] | null>(restoredDraft?.pendingCuts ?? null);
  const [checkedCuts, setCheckedCuts] = useState<ReadonlySet<number>>(() => new Set(restoredDraft?.checked));
  const [preview, setPreview] = useState(false);
  const [audition, setAudition] = useState<TimeRange | null>(null);
  const cleanupJob = useRef<{ snapshot: Segment[]; controller: AbortController; assistant: boolean } | null>(null);
  const mediaRef = useRef<HTMLVideoElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const mediaPanelRef = useRef<HTMLElement>(null);
  const chatRef = useRef<HTMLDivElement>(null);
  const importController = useRef<AbortController | null>(null);
  const scrubPointer = useRef<{ id: number; offset: number } | null>(null);

  useEffect(() => () => importController.current?.abort(), []);
  useEffect(() => () => cleanupJob.current?.controller.abort(), []);

  const clipById = useMemo(
    () => new Map(project.clips.map((clip) => [clip.id, clip])),
    [project.clips],
  );
  const source = selectedId ? clipById.get(selectedId) : undefined;
  const sourceMarks = selectedId ? marks[selectedId] || {} : {};
  const lastEdit = history[history.length - 1];
  const canUndo = lastEdit?.after === project.timeline;
  const currentReview = cleanupReview?.snapshot === project.timeline && !!cleanupReview.assistant !== cleanupOpen
    ? cleanupReview
    : null;
  const cleanupReport = currentReview?.report ?? null;
  const cleanupEdit = useMemo(() => {
    if (!cleanupReport) return null;
    return makeCleanupEdit(project.timeline, cleanupReport.proposals.flatMap((cut, index) =>
      checkedCuts.has(index) ? [cut.remove] : [],
    ), cleanupReport.canClearTimeline);
  }, [cleanupReport, project.timeline, checkedCuts]);
  const previewing = preview && !!cleanupEdit;
  const timeline = previewing ? cleanupEdit.timeline : project.timeline;
  const total = useMemo(
    () => sequenceLength(timeline),
    [timeline],
  );
  const {
    scrollRef: timelineScrollRef,
    viewport: timelineViewport,
    zoom,
    scale: pixelsPerSecond,
    width: timelineWidth,
    changeZoom,
  } = useTimelineZoom(total, sequenceTime);
  const positioned = useMemo(() => {
    let cursor = 0;
    return timeline.map((segment) => {
      const item = { ...segment, start: cursor };
      cursor += segmentLength(segment);
      return item;
    });
  }, [timeline]);
  const hasSequenceMarks = sequenceMarks.in !== undefined || sequenceMarks.out !== undefined;
  const sequenceRange = previewing ? null : markedRange(sequenceMarks, total);
  const selectedRange = useMemo(() => {
    const segment = positioned.find((item) => item.id === selectedSegment);
    return segment ? { start: segment.start, end: segment.start + segmentLength(segment) } : null;
  }, [positioned, selectedSegment]);
  const editRange = hasSequenceMarks ? sequenceRange : selectedRange;
  const canEditRange = !previewing && !!editRange;
  const segmentAt = useCallback(
    (time: number) =>
      time >= total
        ? positioned[positioned.length - 1]
        : positioned.find(
            (segment) =>
              time >= segment.start &&
              time < segment.start + segmentLength(segment),
          ),
    [positioned, total],
  );
  const currentSegment = segmentAt(sequenceTime);
  const canSplit =
    !previewing && !!currentSegment &&
    sequenceTime >= currentSegment.start + minSegmentDuration &&
    sequenceTime <=
      currentSegment.start + segmentLength(currentSegment) - minSegmentDuration;
  const activeClip =
    mode === 'source'
      ? source
      : currentSegment
        ? clipById.get(currentSegment.clipId)
        : undefined;
  const nextSegment =
    mode === 'sequence' && currentSegment
      ? positioned[positioned.indexOf(currentSegment) + 1]
      : undefined;
  const { urls: linkedUrls, refresh: refreshMedia } = useLinkedMedia(project.clips, activeClip?.filePath);
  useSavedAudio(project.timeline, clipById, importing, linkedUrls, onUpdate);
  const { urls, loading: loadingBlob } = useMediaUrls(
    activeClip,
    nextSegment ? clipById.get(nextSegment.clipId) : undefined,
  );
  const linkedUrl = activeClip?.filePath ? linkedUrls.get(activeClip.filePath) : undefined;
  const mediaLoading = activeClip?.filePath ? linkedUrl === undefined : loadingBlob;
  const activeUrl = activeClip?.filePath
    ? linkedUrl ?? undefined
    : activeClip?.source || (activeClip ? urls[activeClip.id] : undefined);
  const audioSegments = useMemo(
    () => positioned.filter(({ clipId }) => {
      const clip = clipById.get(clipId);
      return (clip?.kind === 'audio' || clip?.hasAudio === true)
        && (!clip.filePath || linkedUrls.get(clip.filePath));
    }),
    [positioned, clipById, linkedUrls],
  );
  const waveformSources = useMemo(
    () => [...new Set(audioSegments.map(({ clipId }) => clipId))].flatMap((id) => {
      const clip = clipById.get(id);
      return clip ? [clip] : [];
    }),
    [audioSegments, clipById],
  );
  const waveforms = useWaveforms(waveformSources);
  const currentTime = mode === 'source' ? sourceTime : sequenceTime;
  const activeDuration = mode === 'source' ? source?.duration || 0 : total;
  const rangeIn = sourceMarks.in ?? 0;
  const rangeOut = sourceMarks.out ?? source?.duration ?? 0;
  const viewerMarks = mode === 'source' ? sourceMarks : sequenceMarks;
  const viewerRangeIn = mode === 'source' ? rangeIn : sequenceMarks.in ?? 0;
  const viewerRangeOut = mode === 'source' ? rangeOut : sequenceMarks.out ?? total;

  useEffect(() => {
    if (linkedUrl === null) {
      setPlaying(false);
      setAudition(null);
    }
  }, [linkedUrl]);

  const advanceSequence = useCallback((media: HTMLVideoElement, updatePlayhead: boolean) => {
    if (!currentSegment || media.seeking || scrubPointer.current) return false;
    const time = currentSegment.start + media.currentTime - currentSegment.in;
    if (audition && time >= audition.end) {
      media.pause();
      setSequenceTime(Math.min(total, audition.end));
      setPlaying(false);
      setAudition(null);
      return true;
    }
    if (media.currentTime >= currentSegment.out - (previewing || audition ? 0 : 0.025)) {
      const end = currentSegment.start + segmentLength(currentSegment);
      if (end >= total - 0.025) {
        setSequenceTime(total);
        setPlaying(false);
        setAudition(null);
      } else {
        setSequenceTime(end);
      }
      return true;
    }
    if (updatePlayhead) setSequenceTime(Math.max(currentSegment.start, Math.min(total, time)));
    return false;
  }, [currentSegment, total, audition, previewing]);

  useEffect(() => {
    if (mode !== 'sequence' || !playing || (!audition && !previewing)) return;
    let frame = requestAnimationFrame(function checkCut() {
      const media = mediaRef.current;
      if (media && !media.paused && advanceSequence(media, false)) return;
      frame = requestAnimationFrame(checkCut);
    });
    return () => cancelAnimationFrame(frame);
  }, [activeUrl, advanceSequence, audition, mode, playing, previewing]);

  useEffect(() => {
    const changedDuringAnalysis = cleanupJob.current && cleanupJob.current.snapshot !== project.timeline;
    const changedAfterAnalysis = cleanupReview && cleanupReview.snapshot !== project.timeline;
    if (!changedDuringAnalysis && !changedAfterAnalysis) return;
    cleanupJob.current?.controller.abort();
    setCleanupReview(null);
    setPendingCuts(null);
    setCheckedCuts(new Set());
    setPreview(false);
    setAudition(null);
    setCleanupError('The timeline changed. Run the analysis again to review suggestions.');
  }, [project.timeline, cleanupReview]);

  useEffect(() => {
    const review = cleanupReview?.snapshot === project.timeline ? cleanupReview : null;
    const pending = pendingCuts === project.timeline ? pendingCuts : null;
    const draft: SavedCleanupDraft | undefined = review || pending
      ? { version: 1, review, checked: [...checkedCuts], pendingCuts: pending }
      : undefined;
    onUpdate((current) => {
      if (current.timeline !== project.timeline || (!draft && !current.cleanupDraft)) return current;
      return { ...current, cleanupDraft: draft };
    }, true);
  }, [cleanupReview, checkedCuts, pendingCuts, project.timeline, onUpdate]);

  useEffect(() => {
    const pauseWhenHidden = () => {
      if (document.hidden) {
        mediaRef.current?.pause();
        setPlaying(false);
      }
    };
    document.addEventListener('visibilitychange', pauseWhenHidden);
    return () =>
      document.removeEventListener('visibilitychange', pauseWhenHidden);
  }, []);
  useEffect(() => {
    if (!notice) return;
    const timeout = setTimeout(() => setNotice(''), 6000);
    return () => clearTimeout(timeout);
  }, [notice]);
  useEffect(() => {
    chatRef.current?.scrollTo({
      top: chatRef.current.scrollHeight,
      behavior: 'smooth',
    });
  }, [project.messages.length]);
  useEffect(() => {
    const media = mediaRef.current;
    if (!media || !activeUrl) return;
    // Explicitly release the decoder on a source change or editor close.
    // Restore the source when React replays effects in development mode.
    if (media.getAttribute('src') !== activeUrl) media.src = activeUrl;
    return () => {
      media.pause();
      media.removeAttribute('src');
      media.load();
    };
  }, [activeUrl, activeClip?.id, mode]);
  useEffect(() => {
    const media = mediaRef.current;
    if (!media || !activeUrl) return;
    let cancelled = false;
    if (playing)
      void media.play().catch(() => {
        if (cancelled) return;
        setPlaying(false);
        setNotice('This media cannot be played in the current browser.');
      });
    else media.pause();
    return () => {
      cancelled = true;
    };
  }, [playing, activeUrl, currentSegment?.id, mode]);

  useEffect(() => {
    const media = mediaRef.current;
    if (!media || media.readyState < 1) return;
    media.currentTime =
      mode === 'source'
        ? sourceTime
        : currentSegment
          ? currentSegment.in + sequenceTime - currentSegment.start
          : 0;
    // Seek when switching viewers or crossing a cut, including two ranges from the same file.
    // Continuous playback updates the playhead through onTimeUpdate instead.
  }, [mode, activeClip?.id, currentSegment?.id, currentSegment?.in, audition, previewing]);

  const selectSource = useCallback(
    (clip: Clip) => {
      setPlaying(false);
      setAudition(null);
      if (mode === 'source' && activeClip?.id === clip.id && mediaRef.current)
        mediaRef.current.currentTime = 0;
      setSelectedId(clip.id);
      setSourceTime(0);
      setMode('source');
    },
    [mode, activeClip?.id],
  );
  const seek = useCallback(
    (value: number, targetMode = mode) => {
      setAudition(null);
      const duration = targetMode === 'source' ? source?.duration || 0 : total;
      const time = Math.max(0, Math.min(value, duration));
      if (targetMode !== mode) setPlaying(false);
      setMode(targetMode);
      if (targetMode === 'source') setSourceTime(time);
      else setSequenceTime(time);
      const segment = targetMode === 'sequence' ? segmentAt(time) : undefined;
      const targetClip =
        targetMode === 'source'
          ? source
          : segment
            ? clipById.get(segment.clipId)
            : undefined;
      if (mediaRef.current && targetClip?.id === activeClip?.id)
        mediaRef.current.currentTime =
          targetMode === 'source'
            ? time
            : segment
              ? segment.in + time - segment.start
              : 0;
    },
    [mode, source, total, segmentAt, clipById, activeClip?.id],
  );
  function startTimelineScrub(event: ReactPointerEvent<HTMLDivElement>) {
    if (!event.isPrimary || event.button !== 0 || !total) return;
    const target = event.target as Element;
    const handle = target.closest('.timeline-playhead-handle');
    if (!handle && !target.closest('.timeline-ruler')) return;

    const x = event.clientX - event.currentTarget.getBoundingClientRect().left;
    // Keep the pointer at the same place on the grip when it is grabbed off center.
    const offset = handle ? x - sequenceTime * pixelsPerSecond : 0;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    scrubPointer.current = { id: event.pointerId, offset };
    mediaRef.current?.pause();
    setPlaying(false);
    seek(handle ? sequenceTime : x / pixelsPerSecond, 'sequence');
  }
  function moveTimelineScrub(event: ReactPointerEvent<HTMLDivElement>) {
    const pointer = scrubPointer.current;
    if (pointer?.id !== event.pointerId) return;
    const left = event.currentTarget.getBoundingClientRect().left;
    seek((event.clientX - left - pointer.offset) / pixelsPerSecond, 'sequence');
  }
  function endTimelineScrub(event: ReactPointerEvent<HTMLDivElement>) {
    if (scrubPointer.current?.id === event.pointerId)
      scrubPointer.current = null;
  }
  function togglePlay() {
    if (!activeClip || !activeUrl) return;
    if (currentTime >= activeDuration - 0.02) seek(0);
    setPlaying((value) => !value);
  }
  function mark(type: 'in' | 'out', targetMode = mode) {
    if (targetMode === 'sequence') {
      if (!total || previewing) return;
      setSequenceMarks((previous) => setRangeMark(previous, type, sequenceTime));
      if (mode !== 'sequence') seek(sequenceTime, 'sequence');
      return;
    }
    if (!source) return;
    setMarks((existing) => {
      return {
        ...existing,
        [source.id]: setRangeMark(existing[source.id] || {}, type, sourceTime),
      };
    });
    if (mode !== 'source') {
      setPlaying(false);
      setMode('source');
    }
  }
  function clearMarks(targetMode = mode) {
    if (targetMode === 'sequence') setSequenceMarks({});
    else if (selectedId) setMarks((existing) => ({ ...existing, [selectedId]: {} }));
  }
  function changeTimeline(next: Segment[], expected?: Segment[], draft?: SavedCleanupDraft) {
    const cleanup: SavedCleanupDraft | null = cleanupReview?.snapshot === project.timeline || pendingCuts === project.timeline
      ? { version: 1, review: cleanupReview?.snapshot === project.timeline ? cleanupReview : null, checked: [...checkedCuts], pendingCuts: pendingCuts === project.timeline ? pendingCuts : null }
      : null;
    setHistory((previous) => [...previous.slice(-29), { before: project.timeline, after: next, cleanup }]);
    setSequenceMarks({});
    onUpdate((current) => expected && current.timeline !== expected ? current : { ...current, timeline: next, cleanupDraft: draft });
  }
  function insert(clip = source, position = sequenceTime) {
    if (!clip || previewing) return;
    const points = marks[clip.id] || {};
    const start = points.in ?? 0;
    const end = points.out ?? clip.duration;
    if (end - start < minSegmentDuration) {
      setNotice('Set an out point after the in point to insert a selection.');
      return;
    }
    const segment = { id: uid(), clipId: clip.id, in: start, out: end };
    changeTimeline(insertSegment(project.timeline, segment, position));
    setSelectedSegment(segment.id);
    setSequenceTime(position + end - start);
    setPlaying(false);
    setNotice(
      `Added ${shortTime(end - start)} from ${clip.name} to the sequence.`,
    );
  }
  function splitAtPlayhead() {
    if (!currentSegment || !canSplit) return;
    const index = positioned.indexOf(currentSegment);
    const segment = project.timeline[index];
    const sourceTime = segment.in + (sequenceTime - currentSegment.start);
    const [left, right] = splitSegment(segment, sourceTime);
    changeTimeline([
      ...project.timeline.slice(0, index),
      left,
      right,
      ...project.timeline.slice(index + 1),
    ]);
    setSelectedSegment(right.id);
    setPlaying(false);
    seek(sequenceTime, 'sequence');
  }
  function undo() {
    if (previewing || !canUndo || !lastEdit) return;
    const { before, after } = lastEdit;
    onUpdate((current) => current.timeline === after ? { ...current, timeline: before, cleanupDraft: lastEdit.cleanup ?? undefined } : current);
    setCleanupReview(lastEdit.cleanup?.review ?? null);
    setCheckedCuts(new Set(lastEdit.cleanup?.checked));
    setPendingCuts(lastEdit.cleanup?.pendingCuts ?? null);
    setCleanupError('');
    setAudition(null);
    setHistory((items) => items.slice(0, -1));
    setSequenceTime((time) => Math.min(time, sequenceLength(before)));
    setPlaying(false);
    setSelectedSegment(null);
    setSequenceMarks({});
  }
  function editSelection(replace = false) {
    if (!editRange || previewing || (replace && !source)) return;
    let replacement: Segment | undefined;
    if (replace && source) {
      if (rangeOut - rangeIn < minSegmentDuration) {
        setNotice('Set a source out point at least one frame after its in point.');
        return;
      }
      replacement = { id: uid(), clipId: source.id, in: rangeIn, out: rangeOut };
    }
    const next = editTimelineRange(project.timeline, editRange, replacement);
    if (!next) {
      setNotice('Move the timeline marks to leave at least one frame on either side of the cut.');
      return;
    }
    mediaRef.current?.pause();
    changeTimeline(next);
    setSelectedSegment(replacement?.id ?? null);
    setSequenceTime(Math.min(editRange.start, sequenceLength(next)));
    setMode('sequence');
    setAudition(null);
    setPlaying(false);
    setNotice(replacement
      ? `Replaced selection with ${source!.name}. Later clips shift to fit. Undo to restore.`
      : 'Deleted selection and closed the gap. Undo to restore.');
  }

  function resetCleanupPreview() {
    if (!previewing && !audition) return;
    setPreview(false);
    setAudition(null);
    setPlaying(false);
    if (previewing) setSequenceTime(0);
  }

  function closeCleanup() {
    if (cleanupOpen) {
      cleanupJob.current?.controller.abort();
      resetCleanupPreview();
      setCleanupError('');
    }
    setCleanupOpen(false);
  }

  function leaveAssistant() {
    if (cleanupJob.current?.assistant) cleanupJob.current.controller.abort();
    if (!cleanupOpen) resetCleanupPreview();
    setCleanupError('');
  }

  async function runCleanup(options: CleanupOptions, snapshot = project.timeline) {
    if (cleanupJob.current) return;
    const stage = options.assistant?.stage;
    const job = { snapshot, controller: new AbortController(), assistant: !!stage };
    cleanupJob.current = job;
    if (stage !== 'chat' && snapshot === project.timeline) resetCleanupPreview();
    if (stage === 'cuts') setPendingCuts(snapshot);
    if (stage === 'pauses' || stage === 'draft') setPendingCuts(null);
    setCleanupError('');
    setCleanupProgress({ stage: stage ? 'Preparing the timeline assistant' : 'Preparing timeline cleanup', fraction: null });
    try {
      const result = await analyzeTimeline(project.id, job.snapshot, clipById, options, job.controller.signal, setCleanupProgress);
      const report = prepareCleanupReview(job.snapshot, result);
      if (cleanupJob.current !== job || job.controller.signal.aborted) return;
      if (stage === 'draft' && !canPreviewCleanupDraft(snapshot, report)) {
        throw new Error('The cleanup draft could not be applied safely to this timeline. Try a more specific brief.');
      }
      const turn = report.assistantTurn;
      if (turn) setAssistantMessages((messages) => [...messages, ...turn]);
      if (stage !== 'chat') {
        setCleanupReview({ snapshot: job.snapshot, report, assistant: stage });
        setCheckedCuts(new Set());
      }
      if (stage === 'cuts') setPendingCuts(null);
    } catch (error) {
      if (!job.controller.signal.aborted) setCleanupError(errorMessage(error, 'Could not analyze the timeline.'));
    } finally {
      if (cleanupJob.current === job) {
        cleanupJob.current = null;
        setCleanupProgress(null);
      }
    }
  }

  function runAssistant(request: AssistantRequest, snapshot = project.timeline) {
    void runCleanup({ pauses: false, retakes: null, assistant: request }, snapshot);
  }

  function cancelCleanup() {
    cleanupJob.current?.controller.abort();
    setCleanupProgress({ stage: 'Stopping analysis…', fraction: null });
  }

  function playCleanupRange(range: TimeRange, edited: boolean) {
    setPreview(edited);
    setMode('sequence');
    setSequenceTime(range.start);
    setAudition({ ...range });
    setPlaying(true);
  }

  function previewCleanup(range?: TimeRange) {
    if (!cleanupEdit) return;
    if (!range && previewing) {
      resetCleanupPreview();
      return;
    }
    const join = cleanupEdit.mapTime(range?.start ?? 0);
    const start = Math.max(0, join - 1.2);
    setSelectedSegment(null);
    if (range) {
      playCleanupRange({ start, end: join + 1.2 }, true);
    } else {
      setPreview(true);
      setMode('sequence');
      setSequenceTime(start);
      setAudition(null);
      setPlaying(false);
    }
  }

  function applyCleanup(nextAssistantStage?: AssistantRequest) {
    if (!cleanupEdit || !currentReview || cleanupJob.current) return;
    const position = previewing ? sequenceTime : cleanupEdit.mapTime(sequenceTime);
    const remaining = nextAssistantStage ? null : remainingCleanupReview(currentReview, checkedCuts, cleanupEdit);
    changeTimeline(cleanupEdit.timeline, currentReview.snapshot, remaining
      ? { version: 1, review: remaining, checked: [], pendingCuts: null } : undefined);
    setCleanupReview(remaining);
    setCheckedCuts(new Set());
    setPreview(false);
    setAudition(null);
    setSelectedSegment(null);
    setMode('sequence');
    setSequenceTime(position);
    setPlaying(false);
    if (!currentReview.assistant && !remaining) setCleanupOpen(false);
    if (nextAssistantStage) runAssistant(nextAssistantStage, cleanupEdit.timeline);
    setNotice(remaining
      ? `Selected cuts applied. ${remaining.report.proposals.length} suggestions remain. Undo restores the previous draft and timeline.`
      : 'Timeline cleanup applied. Undo to restore the original sequence.');
  }

  function selectCutKind(pauses: boolean) {
    resetCleanupPreview();
    setCheckedCuts((checked) => new Set([
      ...checked,
      ...(cleanupReport?.proposals.flatMap((cut, index) => (cut.kind === 'pause') === pauses ? [index] : []) ?? []),
    ]));
  }

  const cutReview = {
    checked: checkedCuts,
    saving: cleanupEdit?.secondsRemoved ?? 0,
    invalidSelection: checkedCuts.size > 0 && !cleanupEdit,
    previewing,
    onToggle(index: number) {
      resetCleanupPreview();
      setCheckedCuts((checked) => {
        const next = new Set(checked);
        if (next.has(index)) next.delete(index);
        else next.add(index);
        return next;
      });
    },
    onSelectPauses() {
      selectCutKind(true);
    },
    onSelectRetakes() {
      selectCutKind(false);
    },
    onClearSelection() {
      resetCleanupPreview();
      setCheckedCuts(new Set());
    },
    onAudition: (range: TimeRange) => playCleanupRange(range, false),
    onPreview: previewCleanup,
    onApply: () => applyCleanup(),
  };
  const hasTimelineAudio = project.timeline.some((segment) => hasAnalyzableAudio(clipById.get(segment.clipId)));
  function jump(direction: -1 | 1) {
    const points =
      mode === 'sequence'
        ? [0, ...positioned.map((segment) => segment.start), total]
        : [0, sourceMarks.in, sourceMarks.out, source?.duration].filter(
            (p): p is number => p !== undefined,
          );
    const sorted = [...new Set(points)].sort((a, b) => a - b);
    const next =
      direction < 0
        ? [...sorted].reverse().find((point) => point < currentTime - 0.04)
        : sorted.find((point) => point > currentTime + 0.04);
    if (next !== undefined) seek(next);
  }
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement;
      if (
        target.closest(
          'input:not([type="range"]), textarea, select, [contenteditable="true"]',
        ) ||
        document.querySelector('[role="dialog"]')
      )
        return;
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'z') {
        event.preventDefault();
        undo();
        return;
      }
      if (event.ctrlKey || event.metaKey || event.altKey) return;
      const key = event.key.toLowerCase();
      const actions: Record<string, () => void> = {
        ' ': togglePlay,
        i: () => mark('in'),
        o: () => mark('out'),
        p: () => clearMarks(),
        v: () => insert(),
        b: () => editSelection(true),
        x: splitAtPlayhead,
        a: () => jump(-1),
        s: () => jump(1),
        arrowleft: () => seek(currentTime - (event.shiftKey ? 10 : 1) / 24),
        arrowright: () => seek(currentTime + (event.shiftKey ? 10 : 1) / 24),
        delete: () => editSelection(),
        backspace: () => editSelection(),
        '?': onShortcuts,
      };
      if (actions[key]) {
        event.preventDefault();
        actions[key]();
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  });

  const importFiles = useCallback(async (files?: (File | string)[]) => {
    if (importController.current) return;
    const controller = new AbortController();
    importController.current = controller;
    setImporting(true);
    setPlaying(false);
    const memoryGiB = (navigator as Navigator & { deviceMemory?: number })
      .deviceMemory;
    try {
      const list = files ?? await invoke<string[]>('pick_media_files');
      controller.signal.throwIfAborted();
      if (!list.length) return;
      const paths = list.filter((file): file is string => typeof file === 'string');
      const originals = paths.length ? await inspectMediaPaths(paths) : {};
      const sizes = list.map((file) =>
        typeof file === 'string' ? { size: originals[file]?.size ?? 0 } : file,
      );
      const results = await mapConcurrent(
        list,
        importConcurrency(sizes, navigator.hardwareConcurrency, memoryGiB),
        async (file) => {
          if (typeof file === 'string') {
            const original = originals[file];
            if (!original) throw new Error('Asset unavailable or unsupported');
            return inspectMedia({ ...original, filePath: file }, controller.signal);
          }
          if (!(
            file.type.startsWith('video/') ||
            file.type.startsWith('audio/') ||
            /\.(mp4|mov|m4v|webm|mkv|avi|mp3|wav|m4a|aac|ogg|flac)$/i.test(
              file.name,
            )
          )) {
            throw new Error('Unsupported file');
          }
          const clip = await inspectMedia(file, controller.signal);
          controller.signal.throwIfAborted();
          await storeMedia(clip.id, file);
          return clip;
        },
        controller.signal,
      );
      const imported = results.flatMap((result) =>
        result.status === 'fulfilled' ? [result.value] : [],
      );
      // Keep successfully stored files even if the editor was closed mid-import.
      if (imported.length)
        onUpdate((current) => ({
          ...current,
          clips: [...current.clips, ...imported],
        }));
      if (controller.signal.aborted) return;
      if (imported.length) {
        setSearch('');
        setFilter('all');
        selectSource(imported[0]);
      }
      const failed = results.flatMap((result, index) => {
        const file = list[index];
        return result.status === 'rejected'
          ? [typeof file === 'string' ? file.split(/[/\\]/).pop() : file.name]
          : [];
      });
      setNotice(
        `${imported.length ? `Imported ${imported.length} clip${imported.length === 1 ? '' : 's'}. ` : ''}${failed.length ? `Could not import ${failed.join(', ')}. Check format and available storage.` : ''}`,
      );
    } catch (error) {
      if (!controller.signal.aborted) setNotice(errorMessage(error, 'Could not import clips.'));
    } finally {
      importController.current = null;
      if (!controller.signal.aborted) {
        setImporting(false);
        if (inputRef.current) inputRef.current.value = '';
      }
    }
  }, [onUpdate, selectSource]);

  function openImport() {
    if (desktop) void importFiles();
    else inputRef.current?.click();
  }

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    const listener = getCurrentWebview().onDragDropEvent(({ payload }) => {
      if (!active) return;
      if (payload.type === 'leave') {
        setDropping(false);
        return;
      }
      const bounds = mediaPanelRef.current?.getBoundingClientRect();
      const x = payload.position.x / window.devicePixelRatio;
      const y = payload.position.y / window.devicePixelRatio;
      const inside = !!bounds && x >= bounds.left && x < bounds.right && y >= bounds.top && y < bounds.bottom;
      setDropping(inside && payload.type !== 'drop');
      if (inside && payload.type === 'drop') void importFiles(payload.paths);
    });
    void listener.catch(() => {});
    return () => {
      active = false;
      void listener.then((unlisten) => unlisten()).catch(() => {});
    };
  }, [desktop, importFiles]);
  function sendPrompt(text: string) {
    if (!text.trim()) return;
    const lower = text.toLowerCase();
    const clipCount = project.clips.length;
    const segmentCount = project.timeline.length;
    let response: string;
    if (/summari|overview|project/.test(lower))
      response = `“${project.name}” has ${clipCount} source ${clipCount === 1 ? 'clip' : 'clips'} and ${segmentCount} timeline ${segmentCount === 1 ? 'segment' : 'segments'}, with a running time of ${shortTime(total)}. ${source ? `You’re previewing “${source.name}” (${shortTime(source.duration)}).` : 'Import a clip to get started.'}`;
    else if (/short|trim|tight|pace/.test(lower))
      response = total
        ? `For a tighter cut, try keeping each shot to 3–5 seconds. Your sequence is currently ${shortTime(total)}. Preview a source clip, use I and O to choose its strongest moment, then press V to insert it. You can select and delete a longer segment first.`
        : 'Start with one strong opening shot. Select a clip, mark a 3–5 second range with I and O, then insert it with V.';
    else if (/next|start|help/.test(lower))
      response = project.clips.length
        ? 'Try building a simple three-shot story: an opening wide shot, a closer detail, then a quiet closing moment. Review each clip, mark its in and out, and insert with V. A and S help you move between cuts in the sequence viewer.'
        : 'Start by importing a few video clips. Select a clip to preview it, press I to mark the start, O to mark the end, and V to insert your selection into the timeline.';
    else
      response =
        'Try “Summarize my project”, “Suggest a tighter cut”, or “What should I do next?” This demo uses the clips and durations listed in your project.';
    const messages = [
      { id: uid(), role: 'user', text: text.trim() },
      { id: uid(), role: 'assistant', text: response },
    ] as const;
    onUpdate((current) => ({
      ...current,
      messages: [...current.messages, ...messages],
    }));
    setPrompt('');
  }
  const visible = useMemo(() => {
    const query = search.toLowerCase();
    return project.clips
      .filter(
        (clip) =>
          (filter === 'all' || clip.kind === filter) &&
          clip.name.toLowerCase().includes(query),
      )
      .sort((a, b) =>
        sort === 'duration'
          ? a.duration - b.duration
          : sort === 'newest'
            ? b.addedAt - a.addedAt
            : a.name.localeCompare(b.name),
      );
  }, [project.clips, filter, search, sort]);
  const clipCards = useMemo(
    () =>
      visible.map((clip) => (
        <button
          key={clip.id}
          className={`clip-card ${selectedId === clip.id ? 'selected' : ''}`}
          draggable
          onDragStart={(event) => {
            event.dataTransfer.setData('application/cutline-clip', clip.id);
            event.dataTransfer.effectAllowed = 'copy';
          }}
          onClick={() => selectSource(clip)}
          title={clip.filePath ?? clip.name}
        >
          <div className="clip-cover">
            <Thumbnail clip={clip} />
            <span className="clip-type">
              <Icon
                name={clip.kind === 'audio' ? 'music' : 'video'}
                size={12}
              />
            </span>
            <span className="clip-duration">{shortTime(clip.duration)}</span>
            {selectedId === clip.id && <span className="clip-selected-dot" />}
          </div>
          <div className="clip-info">
            <span>{clip.name}</span>
            <small>
              {clip.filePath && linkedUrls.get(clip.filePath) === null ? 'Asset unavailable' : (
                <>
                  {clip.width ? `${clip.width} × ${clip.height}` : 'Audio'}{' '}
                  <span>·</span> {shortTime(clip.duration)}
                </>
              )}
            </small>
          </div>
        </button>
      )),
    [visible, selectedId, selectSource, linkedUrls],
  );
  const segmentButtons = useMemo(
    () =>
      Object.fromEntries(
        (['video', 'audio'] as const).map((kind) => [
          kind,
          positioned.flatMap((segment) => {
            const clip = clipById.get(segment.clipId);
            const linked =
              kind === 'audio' && clip?.kind === 'video' && clip.hasAudio === true;
            if (!clip || (clip.kind !== kind && !linked)) return [];
            const missing = !!clip.filePath && linkedUrls.get(clip.filePath) === null;
            const label = `${linked ? 'Linked audio: ' : ''}${clip.name} · ${timecode(segment.in)} – ${timecode(segment.out)}${missing ? ' · Asset unavailable' : ''}`;
            const waveform = kind === 'audio' ? waveforms.get(clip.id) : undefined;
            const waveformError =
              waveform && 'error' in waveform ? waveform.error : undefined;
            return [
              <button
                key={segment.id}
                className={`timeline-segment ${kind} ${selectedSegment === segment.id ? 'selected' : ''}`}
                style={{
                  left: segment.start * pixelsPerSecond,
                  width: segmentLength(segment) * pixelsPerSecond,
                }}
                onClick={(event) => {
                  setSelectedSegment(segment.id);
                  setPlaying(false);
                  let time = segment.start;
                  if (
                    kind === 'audio' &&
                    event.detail > 0 &&
                    !(event.target as Element).closest('.segment-label')
                  ) {
                    const left = event.currentTarget.getBoundingClientRect().left;
                    time += Math.max(
                      0,
                      Math.min(
                        segmentLength(segment) - 0.006,
                        (event.clientX - left) / pixelsPerSecond,
                      ),
                    );
                  }
                  seek(time, 'sequence');
                }}
                aria-label={label}
                aria-pressed={selectedSegment === segment.id}
                title={waveformError ? `${label}\n${waveformError}` : label}
              >
                <div className="segment-label">
                  <Icon
                    name={kind === 'video' ? 'video' : linked ? 'clip' : 'music'}
                    size={11}
                  />
                  <span>{missing ? `${clip.name} · Asset unavailable` : clip.name}</span>
                </div>
                {missing ? null : kind === 'audio' ? (
                  waveform && !waveformError ? null : (
                    <div className="audio-segment-fill">
                      <Icon name="volume" size={14} />
                      <span>
                        {waveformError ? 'Waveform unavailable' : 'Analyzing waveform…'}
                      </span>
                    </div>
                  )
                ) : clip.thumbnail ? (
                  <div
                    className="segment-frames"
                    style={{
                      backgroundImage: `url(${clip.thumbnail})`,
                    }}
                  />
                ) : (
                  <div className="audio-segment-fill">
                    <Icon name="video" size={16} />
                    <span>{shortTime(segmentLength(segment))}</span>
                  </div>
                )}
                <span className="segment-trim-handle left" />
                <span className="segment-trim-handle right" />
              </button>,
            ];
          }),
        ]),
      ),
    [positioned, clipById, selectedSegment, pixelsPerSecond, seek, waveforms, linkedUrls],
  );

  return (
    <div className={`editor-shell ${!aiOpen && !cleanupOpen && !exportOpen ? 'ai-collapsed' : ''}`}>
      <header className="editor-topbar">
        <div className="editor-brand">
          <button onClick={onHome} aria-label="Back to projects" disabled={exportBusy}>
            <Brand />
          </button>
          <span className="topbar-divider" />
          <button className="back-home" onClick={onHome} disabled={exportBusy}>
            <Icon name="back" size={15} /> Projects
          </button>
          <Icon name="chevron" size={13} />
          <span className="editor-project-name">{project.name}</span>
          {project.demo && <span className="small-tag">Sample</span>}
        </div>
        <div className="editor-header-right">
          <span
            className={`save-status ${cacheError ? 'save-error' : ''}`}
            title={cacheError || 'Saved automatically on this device'}
          >
            <Icon name={cacheError ? 'help' : 'check'} size={14} />
            {cacheError ? 'Save failed' : 'All changes saved'}
          </span>
          <button className="button subtle" onClick={onShortcuts}>
            <Icon name="keyboard" size={16} />
            <span>Shortcuts</span>
          </button>
          {isTauri() && (
            <Tool icon="settings" label="Settings" onClick={() => onSettings()} />
          )}
          <button
            className={`button ai-toggle ${aiOpen && !cleanupOpen ? 'active' : ''}`}
            onClick={() => {
              setExportOpen(false);
              if (cleanupOpen) {
                closeCleanup();
                setAiOpen(true);
              } else {
                if (aiOpen) leaveAssistant();
                setAiOpen((value) => !value);
              }
            }}
            aria-expanded={aiOpen && !cleanupOpen}
            disabled={exportBusy}
          >
            <Icon name="sparkles" size={16} /> Assistant
          </button>
          <button
            className={`button export-toggle ${exportOpen ? 'active' : ''}`}
            aria-expanded={exportOpen}
            onClick={() => {
              closeCleanup();
              leaveAssistant();
              setAiOpen(false);
              setExportOpen(true);
              setPlaying(false);
            }}
          >
            <Icon name="film" size={16} /> Export
          </button>
        </div>
      </header>
      {cacheError && (
        <div className="editor-error error-banner" role="alert">
          {cacheError}
        </div>
      )}
      <div className="editor-main">
        <section
          className={`media-panel ${dropping ? 'is-dropping' : ''}`}
          aria-label="Project media"
          ref={mediaPanelRef}
          onDragOver={(event) => {
            if (!desktop && event.dataTransfer.types.includes('Files')) {
              event.preventDefault();
              setDropping(true);
            }
          }}
          onDragLeave={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget as Node))
              setDropping(false);
          }}
          onDrop={(event) => {
            event.preventDefault();
            setDropping(false);
            if (!desktop && event.dataTransfer.files.length)
              void importFiles(Array.from(event.dataTransfer.files));
          }}
        >
          <div className="panel-heading">
            <h2>
              <Icon name="folder" size={16} /> Project media{' '}
              <span>{project.clips.length}</span>
            </h2>
            <Tool
              icon="plus"
              label="Import clips"
              onClick={openImport}
              disabled={importing}
            />
          </div>
          <div className="media-tabs">
            {[
              ['all', 'All clips'],
              ['video', 'Video'],
              ['audio', 'Audio'],
            ].map(([value, label]) => (
              <button
                key={value}
                onClick={() => setFilter(value)}
                className={filter === value ? 'active' : ''}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="media-search">
            <label className="search-box">
              <Icon name="search" size={15} />
              <input
                aria-label="Search clips"
                placeholder="Find a clip..."
                value={search}
                onChange={(event) => setSearch(event.target.value)}
              />
            </label>
          </div>
          <div className="media-controls">
            <label className="sort-select">
              <Icon name="sort" size={14} />
              <select
                value={sort}
                aria-label="Sort clips"
                onChange={(event) => setSort(event.target.value)}
              >
                <option value="name">Name A–Z</option>
                <option value="newest">Newest first</option>
                <option value="duration">Duration</option>
              </select>
            </label>
            <div className="view-switch">
              <button
                aria-label="Clip grid view"
                className={layout === 'grid' ? 'selected' : ''}
                onClick={() => setLayout('grid')}
              >
                <Icon name="grid" size={13} />
              </button>
              <button
                aria-label="Clip list view"
                className={layout === 'list' ? 'selected' : ''}
                onClick={() => setLayout('list')}
              >
                <Icon name="list" size={15} />
              </button>
            </div>
          </div>
          <div className={`clip-grid ${layout === 'list' ? 'clip-list' : ''}`}>
            {clipCards}
          </div>
          {!visible.length && (
            <div className="media-empty">
              <Icon name={search ? 'search' : 'film'} size={32} />
              <h3>
                {search
                  ? 'No matching clips'
                  : filter !== 'all'
                    ? `No ${filter} clips`
                    : 'Your story starts here'}
              </h3>
              <p>
                {search
                  ? 'Try another name.'
                  : 'Import a few clips to find your first frame.'}
              </p>
            </div>
          )}
          <button
            className="import-zone"
            onClick={openImport}
            disabled={importing}
          >
            <Icon name="upload" size={20} />
            <strong>
              {importing ? 'Importing your clips...' : 'Import clips'}
            </strong>
            <span>or drop your files here</span>
          </button>
          <div className="media-footer">
            <span>{project.clips.length} clips</span>
            <span>
              <span className="status-dot" /> {desktop ? 'Local media' : 'Stored locally'}
            </span>
          </div>
          {!desktop && (
            <input
              ref={inputRef}
              type="file"
              multiple
              accept="video/*,audio/*,.mkv,.mov,.m4v"
              className="file-input"
              onChange={(event) => {
                if (event.target.files) void importFiles(Array.from(event.target.files));
              }}
            />
          )}
        </section>
        <section className="viewer-panel" aria-label="Clip viewer">
          <div className="viewer-heading">
            <div className="viewer-tabs">
              <button
                className={mode === 'source' ? 'active' : ''}
                onClick={() => {
                  setPlaying(false);
                  setMode('source');
                }}
              >
                Source <span>{source ? '01' : '–'}</span>
              </button>
              <button
                className={mode === 'sequence' ? 'active' : ''}
                onClick={() => {
                  setPlaying(false);
                  setMode('sequence');
                }}
              >
                Sequence{' '}
                <span>
                  {project.timeline.length.toString().padStart(2, '0')}
                </span>
              </button>
            </div>
            <span className="viewer-format">
              {activeClip?.width
                ? `${activeClip.width} × ${activeClip.height}`
                : 'Preview'}
              <span>24 fps</span>
            </span>
          </div>
          <div className="viewer-title">
            <span>
              <Icon
                name={activeClip?.kind === 'audio' ? 'music' : 'video'}
                size={14}
              />
              {mode === 'source'
                ? source?.name || 'No clip selected'
                : 'Sequence 01'}
            </span>
            {project.demo && <span>Sample footage</span>}
          </div>
          <div className="viewer-stage" ref={stageRef}>
            {activeClip && activeUrl ? (
              <>
                <video
                  key={`${activeClip.id}-${mode}`}
                  ref={mediaRef}
                  crossOrigin={activeClip.filePath ? 'anonymous' : undefined}
                  src={activeUrl}
                  poster={activeClip.thumbnail}
                  muted={muted}
                  playsInline
                  preload="metadata"
                  className={activeClip.kind === 'audio' ? 'audio-media' : ''}
                  onLoadedMetadata={() => {
                    const media = mediaRef.current;
                    if (media)
                      media.currentTime =
                        mode === 'source'
                          ? sourceTime
                          : currentSegment
                            ? currentSegment.in +
                              sequenceTime -
                              currentSegment.start
                            : 0;
                  }}
                  onTimeUpdate={() => {
                    const media = mediaRef.current;
                    if (!media || media.seeking) return;
                    if (mode === 'source') setSourceTime(media.currentTime);
                    else if (playing) advanceSequence(media, true);
                  }}
                  onEnded={() => {
                    if (mode === 'source') setPlaying(false);
                    else if (playing && currentSegment && !scrubPointer.current) {
                      const end =
                        currentSegment.start + segmentLength(currentSegment);
                      setSequenceTime(end);
                      if (end >= total - 0.025 || (audition && end >= audition.end)) {
                        setPlaying(false);
                        setAudition(null);
                      }
                    }
                  }}
                  onError={() => {
                    setPlaying(false);
                    if (activeClip.filePath) refreshMedia();
                    setNotice(
                      'This clip could not be played. Check that the source is accessible and its format is supported.',
                    );
                  }}
                />
                {activeClip.kind === 'audio' && (
                  <div className="audio-stage">
                    <Icon name="music" size={54} />
                    <span>{activeClip.name}</span>
                    <small>Audio source</small>
                  </div>
                )}
                <button
                  className={`stage-play ${playing ? 'playing' : ''}`}
                  aria-label={playing ? 'Pause preview' : 'Play preview'}
                  onClick={togglePlay}
                >
                  <Icon name={playing ? 'pause' : 'play'} size={30} />
                </button>
                <div className="stage-label">
                  <span className="status-dot" />
                  {mode === 'source' ? 'SOURCE' : previewing ? 'CLEANUP PREVIEW' : 'SEQUENCE'}
                </div>
              </>
            ) : (
              <div className="viewer-empty">
                <span>
                  <Icon name="monitor" size={35} />
                </span>
                <h3>
                  {activeClip
                    ? mediaLoading
                      ? 'Loading media…'
                      : 'Asset unavailable'
                    : mode === 'sequence'
                      ? 'Make your first cut'
                      : 'Find your opening frame'}
                </h3>
                <p>
                  {activeClip
                    ? mediaLoading
                      ? 'Opening the selected clip.'
                      : activeClip.filePath
                        ? 'Restore the original file to its saved location to resume playback.'
                        : 'Import the source again to add a playable copy.'
                    : mode === 'sequence'
                      ? 'Mark a source clip and insert it into the timeline.'
                      : 'Select a clip from your project media to preview it.'}
                </p>
                {!project.clips.length && (
                  <button
                    className="button secondary"
                    onClick={openImport}
                  >
                    <Icon name="upload" size={15} />
                    Import clips
                  </button>
                )}
              </div>
            )}
          </div>
          <div className="viewer-bottom">
            <div className="viewer-scrub">
              <span className="timecode current">{timecode(currentTime)}</span>
              <div className="scrub-control">
                {!previewing && activeDuration > 0 &&
                  (viewerMarks.in !== undefined || viewerMarks.out !== undefined) && (
                    <div
                      className="marked-range"
                      style={{
                        left: `${(viewerRangeIn / activeDuration) * 100}%`,
                        width: `${(Math.max(0, viewerRangeOut - viewerRangeIn) / activeDuration) * 100}%`,
                      }}
                    />
                  )}
                <input
                  aria-label="Viewer playhead"
                  type="range"
                  min="0"
                  max={activeDuration || 1}
                  step={1 / 24}
                  value={Math.min(currentTime, activeDuration)}
                  disabled={!activeClip}
                  onChange={(event) => seek(Number(event.target.value))}
                />
              </div>
              <span className="timecode">{timecode(activeDuration)}</span>
            </div>
            <div className="transport-row">
              <div className="transport-left">
                <Tool
                  icon={muted ? 'mute' : 'volume'}
                  label={muted ? 'Unmute preview' : 'Mute preview'}
                  onClick={() => setMuted((value) => !value)}
                />
                <span>Fit</span>
              </div>
              <div className="transport-main">
                <Tool
                  icon="previous"
                  label="Previous cut (A)"
                  onClick={() => jump(-1)}
                  disabled={!activeClip}
                />
                <Tool
                  icon="back"
                  label="Previous frame"
                  onClick={() => seek(currentTime - 1 / 24)}
                  disabled={!activeClip}
                />
                <button
                  className="transport-play"
                  onClick={togglePlay}
                  disabled={!activeUrl}
                  aria-label={playing ? 'Pause (Space)' : 'Play (Space)'}
                >
                  <Icon name={playing ? 'pause' : 'play'} size={20} />
                </button>
                <Tool
                  icon="arrow"
                  label="Next frame"
                  onClick={() => seek(currentTime + 1 / 24)}
                  disabled={!activeClip}
                />
                <Tool
                  icon="next"
                  label="Next cut (S)"
                  onClick={() => jump(1)}
                  disabled={!activeClip}
                />
              </div>
              <Tool
                icon="expand"
                label="Fullscreen viewer"
                onClick={() => {
                  if (document.fullscreenElement)
                    void document.exitFullscreen();
                  else
                    void stageRef.current
                      ?.requestFullscreen()
                      .catch(() =>
                        setNotice('Fullscreen is unavailable in this preview.'),
                      );
                }}
              />
            </div>
            <div className="marking-row">
              <div className="marking-controls">
                <button
                  onClick={() => mark('in')}
                  disabled={mode === 'source' ? !source : !total || previewing}
                  title={`Mark ${mode === 'source' ? 'source' : 'timeline'} in (I)`}
                >
                  <Icon name="in" size={15} />
                  <kbd>I</kbd>
                  <span>
                    {viewerMarks.in !== undefined
                      ? timecode(viewerMarks.in)
                      : 'Mark in'}
                  </span>
                </button>
                <button
                  onClick={() => mark('out')}
                  disabled={mode === 'source' ? !source : !total || previewing}
                  title={`Mark ${mode === 'source' ? 'source' : 'timeline'} out (O)`}
                >
                  <Icon name="out" size={15} />
                  <kbd>O</kbd>
                  <span>
                    {viewerMarks.out !== undefined
                      ? timecode(viewerMarks.out)
                      : 'Mark out'}
                  </span>
                </button>
                <Tool
                  icon="clear"
                  label="Clear marks (P)"
                  onClick={() => clearMarks()}
                  disabled={mode === 'source' ? !source : !hasSequenceMarks || previewing}
                />
              </div>
              <button
                className="button primary insert-button"
                onClick={() => insert()}
                disabled={!source || previewing}
              >
                <Icon name="plus" size={15} /> Insert <kbd>V</kbd>
              </button>
            </div>
          </div>
        </section>
        {exportOpen ? (
          <ExportPanel project={project} onClose={() => setExportOpen(false)} onBusyChange={setExportBusy} />
        ) : cleanupOpen ? (
          <CleanupPanel
            providerRevision={providerRevision}
            progress={cleanupProgress}
            error={cleanupError}
            report={cleanupReport}
            hasAudio={hasTimelineAudio}
            onClose={closeCleanup}
            onSettings={onSettings}
            onAnalyze={(options) => void runCleanup(options)}
            onAdaptive={(provider) => {
              setCleanupOpen(false);
              setAiOpen(true);
              runAssistant({ stage: 'draft', message: adaptiveCleanupBrief, provider });
            }}
            onCancel={cancelCleanup}
            {...cutReview}
          />
        ) : aiOpen && desktop ? (
          <AssistantPanel
            projectId={project.id}
            providerRevision={providerRevision}
            hasAudio={hasTimelineAudio}
            progress={cleanupProgress}
            error={cleanupError}
            report={cleanupReport}
            reviewingPauses={currentReview?.assistant === 'pauses'}
            needsCuts={pendingCuts === project.timeline}
            messages={assistantMessages}
            review={cutReview}
            onRun={runAssistant}
            onApplyPauses={applyCleanup}
            onCancel={cancelCleanup}
            onClose={() => {
              leaveAssistant();
              setAiOpen(false);
            }}
            onSettings={onSettings}
          />
        ) : aiOpen && (
          <aside className="ai-panel" aria-label="AI assistant">
            <div className="panel-heading">
              <h2>
                <Icon name="sparkles" size={16} /> Your creative copilot
              </h2>
              <span className="small-tag">DEMO</span>
            </div>
            <div className="ai-context">
              <span className="status-dot" /> Project context{' '}
              <span>
                {project.clips.length} clips · {shortTime(total)}
              </span>
            </div>
            <div className="ai-chat" ref={chatRef}>
              {!project.messages.length ? (
                <div className="ai-welcome">
                  <span className="ai-symbol">
                    <Icon name="sparkles" size={27} />
                  </span>
                  <span className="eyebrow">A SECOND PAIR OF EYES</span>
                  <h2>
                    A little help.
                    <br />A lot of possibility.
                  </h2>
                  <p>
                    Find your rhythm, shape your story, or figure out the next
                    cut.
                  </p>
                  <div className="ai-suggestions">
                    {[
                      ['film', 'Summarize my project'],
                      ['clip', 'Suggest a tighter cut'],
                      ['sparkles', 'What should I do next?'],
                    ].map(([icon, text]) => (
                      <button key={text} onClick={() => sendPrompt(text)}>
                        <Icon name={icon as IconName} size={15} />
                        <span>{text}</span>
                        <Icon name="arrow" size={13} />
                      </button>
                    ))}
                  </div>
                  <p className="ai-demo-note">
                    Local demo · suggestions use project details.
                    <br />
                    Footage analysis comes later.
                  </p>
                </div>
              ) : (
                <div className="chat-messages">
                  {project.messages.map((message) => (
                    <div
                      key={message.id}
                      className={`chat-message ${message.role}`}
                    >
                      <div className="message-role">
                        {message.role === 'assistant' ? (
                          <>
                            <Icon name="sparkles" size={13} /> Cutline
                          </>
                        ) : (
                          'You'
                        )}
                      </div>
                      <p>{message.text}</p>
                    </div>
                  ))}
                </div>
              )}
            </div>
            <form
              className="ai-composer"
              onSubmit={(event) => {
                event.preventDefault();
                sendPrompt(prompt);
              }}
            >
              <label className="sr-only" htmlFor="ai-prompt">
                Ask your creative copilot
              </label>
              <textarea
                id="ai-prompt"
                value={prompt}
                onChange={(event) => setPrompt(event.target.value)}
                placeholder="What are you thinking?"
                rows={2}
                maxLength={1500}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' && !event.shiftKey) {
                    event.preventDefault();
                    sendPrompt(prompt);
                  }
                }}
              />
              <div>
                <span>
                  <Icon name="clip" size={12} /> Current project
                </span>
                <button
                  aria-label="Send message"
                  disabled={!prompt.trim()}
                  type="submit"
                >
                  <Icon name="arrow" size={17} />
                </button>
              </div>
            </form>
            <div className="ai-footer">Your vision. A little assistance.</div>
          </aside>
        )}
      </div>
      <section className="timeline-panel" aria-label="Timeline">
        <div className="timeline-toolbar">
          <div className="timeline-name">
            <Icon name="film" size={16} />
            <h2>Sequence 01</h2>
            <span>{timecode(total)}</span>
          </div>
          <div className="timeline-tools">
            {isTauri() && (
              <button type="button" className="button secondary cleanup-toggle" aria-expanded={cleanupOpen} disabled={exportBusy} onClick={() => {
                setExportOpen(false);
                if (cleanupOpen) closeCleanup();
                else {
                  leaveAssistant();
                  setCleanupOpen(true);
                }
              }}>
                <Icon name="sparkles" size={14} /> Clean up timeline
              </button>
            )}
            <button
              type="button"
              className="icon-button timeline-split"
              aria-label="Split at playhead"
              aria-keyshortcuts="X"
              title="Split at playhead (X)"
              onClick={splitAtPlayhead}
              disabled={!canSplit}
            >
              <Icon name="scissors" size={14} /> Split
            </button>
            <Tool
              icon="undo"
              label="Undo timeline edit"
              onClick={undo}
              disabled={!canUndo || previewing}
            />
            <span className="tool-divider" />
            <button
              type="button"
              className="icon-button timeline-zoom-button"
              aria-label="Zoom out timeline"
              title="Zoom out timeline"
              onClick={() => changeZoom(zoom - 10)}
              disabled={zoom === 0}
            >
              −
            </button>
            <input
              type="range"
              aria-label="Timeline zoom"
              aria-valuetext={zoom === 0 ? 'Fit whole sequence' : undefined}
              min="0"
              max="100"
              value={zoom}
              onChange={(event) => changeZoom(Number(event.target.value))}
              onKeyDown={(event) => {
                if (event.key === 'ArrowLeft' || event.key === 'ArrowRight')
                  event.stopPropagation();
              }}
            />
            <button
              type="button"
              className="icon-button timeline-zoom-button"
              aria-label="Zoom in timeline"
              title="Zoom in timeline"
              onClick={() => changeZoom(zoom + 10)}
              disabled={zoom === 100}
            >
              +
            </button>
            <button
              type="button"
              className="icon-button timeline-fit"
              aria-label="Fit sequence to timeline"
              title="Fit sequence to timeline"
              onClick={() => changeZoom(0)}
              disabled={zoom === 0}
            >
              Fit
            </button>
            <button
              className="button subtle timeline-preview"
              onClick={() => {
                setPlaying(false);
                setMode('sequence');
                seek(0, 'sequence');
              }}
            >
              <Icon name="play" size={13} /> Preview sequence
            </button>
          </div>
        </div>
        <div className="timeline-editbar" role="group" aria-label="Timeline range editing">
          <span className="timeline-range-label">Range</span>
          <button
            className={`timeline-edit-button ${sequenceMarks.in !== undefined ? 'is-active' : ''}`}
            aria-label="Mark timeline in"
            aria-keyshortcuts="I"
            title="Mark timeline in at the playhead (I in Sequence viewer)"
            disabled={!total || previewing}
            onClick={() => mark('in', 'sequence')}
          >
            <Icon name="in" size={14} /> In <kbd>I</kbd>
            {sequenceMarks.in !== undefined && <span className="range-time">{timecode(sequenceMarks.in)}</span>}
          </button>
          <button
            className={`timeline-edit-button ${sequenceMarks.out !== undefined ? 'is-active' : ''}`}
            aria-label="Mark timeline out"
            aria-keyshortcuts="O"
            title="Mark timeline out at the playhead (O in Sequence viewer)"
            disabled={!total || previewing}
            onClick={() => mark('out', 'sequence')}
          >
            <Icon name="out" size={14} /> Out <kbd>O</kbd>
            {sequenceMarks.out !== undefined && <span className="range-time">{timecode(sequenceMarks.out)}</span>}
          </button>
          <button
            className="icon-button"
            aria-label="Clear timeline marks"
            title="Clear timeline marks (P in Sequence viewer)"
            disabled={!hasSequenceMarks || previewing}
            onClick={() => clearMarks('sequence')}
          >
            <Icon name="clear" size={14} />
          </button>
          <span className="timeline-range-summary">
            {previewing ? 'Cleanup preview' : sequenceRange
              ? `${timecode(sequenceRange.end - sequenceRange.start)} selected`
              : hasSequenceMarks ? 'Set a range of at least one frame'
              : selectedRange ? 'Selected clip' : 'Select a clip or mark In / Out'}
          </span>
          <div className="timeline-edit-actions">
            <button
              className="timeline-edit-button"
              aria-label={hasSequenceMarks ? 'Delete marked timeline range' : 'Remove selected segment'}
              aria-keyshortcuts="Delete Backspace"
              title="Delete the marked timeline range or selected clip and close the gap (Delete)"
              disabled={!canEditRange}
              onClick={() => editSelection()}
            >
              <Icon name="trash" size={14} /> Delete
            </button>
            <button
              className="timeline-edit-button"
              aria-label="Replace timeline selection with source"
              aria-keyshortcuts="B"
              title={source
                ? `Replace selection with ${source.name} (${timecode(Math.max(0, rangeOut - rangeIn))}). Later clips shift to fit. (B)`
                : 'Select a source clip to replace the timeline selection (B)'}
              disabled={!canEditRange || !source || rangeOut - rangeIn < minSegmentDuration}
              onClick={() => editSelection(true)}
            >
              <Icon name="replace" size={14} /> Replace <kbd>B</kbd>
            </button>
          </div>
        </div>
        <div className="timeline-body">
          <div className="track-headers">
            <div className="ruler-header">
              <Icon name="monitor" size={13} />
            </div>
            <div className="track-header">
              <span className="track-id video-track">V1</span>
              <span>Video</span>
              <Icon name="video" size={14} />
            </div>
            <div className="track-header">
              <span className="track-id audio-track">A1</span>
              <span>Audio</span>
              <Icon name="volume" size={14} />
            </div>
          </div>
          <div className="timeline-scroll" ref={timelineScrollRef}>
            <div
              className="timeline-content"
              style={{ width: `${timelineWidth}px` }}
              onPointerDown={startTimelineScrub}
              onPointerMove={moveTimelineScrub}
              onPointerUp={(event) => {
                moveTimelineScrub(event);
                endTimelineScrub(event);
              }}
              onPointerCancel={endTimelineScrub}
              onLostPointerCapture={endTimelineScrub}
            >
              <TimelineRuler
                scrollRef={timelineScrollRef}
                viewport={timelineViewport}
                scale={pixelsPerSecond}
                width={timelineWidth}
              />
              {(['video', 'audio'] as const).map((kind) => (
                <div
                  key={kind}
                  className={`timeline-track ${kind}`}
                  onDragOver={(event) => {
                    if (
                      event.dataTransfer.types.includes(
                        'application/cutline-clip',
                      )
                    ) {
                      event.preventDefault();
                      event.dataTransfer.dropEffect = 'copy';
                    }
                  }}
                  onDrop={(event) => {
                    event.preventDefault();
                    const clip = project.clips.find(
                      (item) =>
                        item.id ===
                        event.dataTransfer.getData('application/cutline-clip'),
                    );
                    if (clip) {
                      const rect = event.currentTarget.getBoundingClientRect();
                      insert(
                        clip,
                        Math.min(
                          total,
                          Math.max(
                            0,
                            (event.clientX - rect.left) / pixelsPerSecond,
                          ),
                        ),
                      );
                    }
                  }}
                  onClick={(event) => {
                    if (event.target === event.currentTarget) {
                      const rect = event.currentTarget.getBoundingClientRect();
                      seek(
                        Math.min(
                          total,
                          Math.max(
                            0,
                            (event.clientX - rect.left) / pixelsPerSecond,
                          ),
                        ),
                        'sequence',
                      );
                    }
                  }}
                >
                  {segmentButtons[kind]}
                  {kind === 'audio' && !!audioSegments.length && (
                    <TimelineWaveforms
                      scrollRef={timelineScrollRef}
                      viewport={timelineViewport}
                      scale={pixelsPerSecond}
                      segments={audioSegments}
                      entries={waveforms}
                    />
                  )}
                  {!segmentButtons[kind].length && (
                    <span className="empty-track-label">
                      {kind === 'video'
                        ? 'Drag a clip here, or mark a source and press V'
                        : 'Audio clips and linked video audio appear here'}
                    </span>
                  )}
                </div>
              ))}
              {sequenceRange && (
                <div
                  className="timeline-range-selection"
                  aria-label={`Timeline range ${timecode(sequenceRange.start)} to ${timecode(sequenceRange.end)}`}
                  style={{ left: sequenceRange.start * pixelsPerSecond, width: (sequenceRange.end - sequenceRange.start) * pixelsPerSecond }}
                >
                  <span className="timeline-range-in">I</span>
                  <span className="timeline-range-out">O</span>
                </div>
              )}
              <div
                className="timeline-playhead"
                style={{
                  transform: `translateX(${sequenceTime * pixelsPerSecond}px)`,
                }}
              >
                <span
                  className="timeline-playhead-handle"
                  title="Drag playhead"
                />
                <i />
              </div>
            </div>
          </div>
        </div>
        <div className="editor-statusbar">
          <span>
            <span className="status-dot" />{' '}
            {cacheError ? 'Storage needs attention' : 'Local workspace'}
            <span className="statusbar-divider" />
            {project.clips.length} source clips
          </span>
          <div>
            <span>
              <kbd>I</kbd> In
            </span>
            <span>
              <kbd>O</kbd> Out
            </span>
            <span>
              <kbd>V</kbd> Insert
            </span>
            <span>
              <kbd>Space</kbd> Play
            </span>
            <button onClick={onShortcuts}>
              <Icon name="keyboard" size={13} /> All shortcuts
            </button>
          </div>
          <span>
            24 fps <span className="statusbar-divider" /> 16:9
          </span>
        </div>
      </section>
      {notice && (
        <div className="toast" role="status">
          <Icon name="help" size={16} />
          <span>{notice}</span>
          <button
            onClick={() => setNotice('')}
            aria-label="Dismiss notification"
          >
            <Icon name="close" size={14} />
          </button>
        </div>
      )}
    </div>
  );
}
