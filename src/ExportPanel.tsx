import { useEffect, useRef, useState } from 'react';
import { isTauri } from '@tauri-apps/api/core';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { errorMessage } from './cleanupNative';
import { exportVideo, type ExportProgress, type ExportSettings } from './exportNative';
import { Icon } from './Icon';
import { sequenceLength, shortTime, type Project } from './types';

export function ExportPanel({ project, onClose, onBusyChange }: {
  project: Project;
  onClose: () => void;
  onBusyChange: (busy: boolean) => void;
}) {
  const desktop = isTauri();
  const [resolution, setResolution] = useState('1920x1080');
  const [fps, setFps] = useState(24);
  const [quality, setQuality] = useState<ExportSettings['quality']>('balanced');
  const [progress, setProgress] = useState<ExportProgress | null>(null);
  const [error, setError] = useState('');
  const [result, setResult] = useState<{ path: string; timeline: Project['timeline'] } | null>(null);
  const [cancelled, setCancelled] = useState(false);
  const job = useRef<AbortController | null>(null);
  const mounted = useRef(true);
  const duration = sequenceLength(project.timeline);
  const clips = new Map(project.clips.map((clip) => [clip.id, clip]));
  const firstVideoId = project.timeline.find((segment) => clips.get(segment.clipId)?.kind === 'video')?.clipId;
  const firstVideo = firstVideoId ? clips.get(firstVideoId) : undefined;
  const sourceWidth = Math.max(16, Math.ceil((firstVideo?.width ?? 0) / 2) * 2);
  const sourceHeight = Math.max(16, Math.ceil((firstVideo?.height ?? 0) / 2) * 2);
  const sourceResolution = firstVideo?.width && firstVideo.height && sourceWidth <= 4096 && sourceHeight <= 4096
    && sourceWidth * sourceHeight <= 4096 * 2160 ? `${sourceWidth}x${sourceHeight}` : null;
  const resolutions = new Map([
    ['1280x720', '720p · 1280 × 720'],
    ['1920x1080', '1080p · 1920 × 1080'],
    ['3840x2160', '4K · 3840 × 2160'],
  ]);
  if (sourceResolution && !resolutions.has(sourceResolution)) {
    resolutions.set(sourceResolution, `Source · ${sourceWidth} × ${sourceHeight}`);
  }

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      job.current?.abort();
      onBusyChange(false);
    };
  }, [onBusyChange]);

  async function render() {
    if (job.current) return;
    const controller = new AbortController();
    job.current = controller;
    onBusyChange(true);
    const snapshot = project;
    setError('');
    setResult(null);
    setCancelled(false);
    setProgress({ stage: 'Preparing export', fraction: null });
    const [width, height] = (resolutions.has(resolution) ? resolution : '1920x1080').split('x').map(Number);
    try {
      const path = await exportVideo(snapshot, { width, height, fps, quality }, controller.signal, (next) => {
        if (mounted.current) setProgress(next);
      });
      if (mounted.current && path) setResult({ path, timeline: snapshot.timeline });
    } catch (error) {
      if (mounted.current) {
        if (controller.signal.aborted) setCancelled(true);
        else setError(errorMessage(error, 'Could not export the video.'));
      }
    } finally {
      if (job.current === controller) job.current = null;
      if (mounted.current) {
        onBusyChange(false);
        setProgress(null);
      }
    }
  }

  return (
    <aside className="ai-panel export-panel" aria-label="Export video">
      <div className="panel-heading">
        <h2><Icon name="film" size={16} /> Export</h2>
        <button className="icon-button" aria-label="Close export section" onClick={onClose} disabled={!!progress}>
          <Icon name="close" size={15} />
        </button>
      </div>
      <div className="export-body">
        <div className="export-summary">
          <span className="eyebrow">YOUR FINISHED VIDEO</span>
          <h3>{project.name}</h3>
          <p>{project.timeline.length} cuts · {shortTime(duration)} · MP4</p>
        </div>
        {progress && (
          <div className="export-progress">
            <p role="status">{progress.stage}</p>
            <progress aria-label="Video export progress" max={1} value={progress.fraction ?? undefined} />
            {progress.fraction !== null && <span>{Math.floor(progress.fraction * 100)}%</span>}
            <button className="button secondary" type="button" disabled={job.current?.signal.aborted} onClick={() => {
              job.current?.abort();
              setProgress({ stage: 'Cancelling export…', fraction: null });
            }}>Cancel export</button>
          </div>
        )}
        {error && <p className="cleanup-warning" role="alert">{error}</p>}
        {cancelled && <p className="export-note" role="status">Export cancelled.</p>}
        {result && (
          <div className="export-result">
            <h3><Icon name="check" size={16} /> Export complete</h3>
            <p role="status">{result.path}</p>
            {result.timeline !== project.timeline && <p className="export-note">The timeline has changed since this export started. Export again to include your latest edits.</p>}
            <button className="button secondary" onClick={() => {
              void revealItemInDir(result.path).catch((error) => setError(errorMessage(error, 'Could not open the destination folder.')));
            }}>Show in folder</button>
          </div>
        )}
        <form className="export-form" onSubmit={(event) => { event.preventDefault(); void render(); }}>
          <fieldset disabled={!!progress}>
            <legend className="sr-only">Video export settings</legend>
            <label htmlFor="export-resolution">Resolution</label>
            <select id="export-resolution" value={resolutions.has(resolution) ? resolution : '1920x1080'} onChange={(event) => setResolution(event.target.value)}>
              {[...resolutions].map(([value, label]) => <option key={value} value={value}>{label}</option>)}
            </select>
            <label htmlFor="export-fps">Frame rate</label>
            <select id="export-fps" value={fps} onChange={(event) => setFps(Number(event.target.value))}>
              {[24, 25, 30, 60].map((rate) => <option key={rate} value={rate}>{rate} fps</option>)}
            </select>
            <label htmlFor="export-quality">Quality</label>
            <select id="export-quality" value={quality} onChange={(event) => setQuality(event.target.value as ExportSettings['quality'])}>
              <option value="high">High quality</option>
              <option value="balanced">Balanced</option>
              <option value="smaller">Smaller file</option>
            </select>
          </fieldset>
          {!progress && (
            <button className="button primary" type="submit" disabled={!desktop || !duration}>
              <Icon name="film" size={15} /> {result ? 'Export again' : 'Export MP4'}
            </button>
          )}
          <p className="export-note">H.264 + AAC stereo. Picture fits with letterboxing. Audio-only segments use black picture.</p>
          <p className="export-note">Exports the timeline at the moment you start, including applied cleanup cuts.</p>
          {!desktop && <p className="cleanup-warning">Open Cutline’s desktop app to render video. Export requires FFmpeg and ffprobe on PATH.</p>}
          {desktop && <p className="export-note">Automatically uses NVIDIA hardware encoding when available, with CPU fallback. Temporary files use the destination drive.</p>}
          {!duration && <p className="export-note">Add clips to the timeline to create a video.</p>}
        </form>
      </div>
    </aside>
  );
}
