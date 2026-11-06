import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readMedia } from './cache';
import { uid, type Project } from './types';

export type ExportSettings = {
  width: number;
  height: number;
  fps: number;
  quality: 'high' | 'balanced' | 'smaller';
};
export type ExportProgress = { stage: string; fraction: number | null };
type Preparation = { path: string; sources: string[] };
const chunkBytes = 1024 * 1024;

export async function exportVideo(
  project: Project,
  settings: ExportSettings,
  signal: AbortSignal,
  update: (progress: ExportProgress) => void,
): Promise<string | null> {
  const id = uid();
  const clips = new Map(project.clips.map((clip) => [clip.id, clip]));
  const sources = [...new Set(project.timeline.map((segment) => segment.clipId))].map((clipId) => {
    const clip = clips.get(clipId);
    if (!clip) throw new Error('A timeline source is missing. Restore its media before exporting.');
    return { clipId, kind: clip.kind, filePath: clip.filePath ?? null };
  });
  let cancelling: Promise<unknown> | undefined;
  const cancel = () => {
    cancelling = invoke('export_cancel', { id }).catch(() => {});
  };
  const unlisten = await listen<ExportProgress & { id: string }>('video-export-progress', ({ payload }) => {
    if (payload.id === id && !signal.aborted) update(payload);
  });
  signal.addEventListener('abort', cancel, { once: true });
  let complete = false;
  try {
    signal.throwIfAborted();
    update({ stage: 'Choose where to save your MP4', fraction: null });
    const preparation = await invoke<Preparation | null>('export_begin', {
      id, name: project.name, settings, segments: project.timeline, sources,
    });
    signal.throwIfAborted();
    if (!preparation) return null;
    for (const [index, clipId] of preparation.sources.entries()) {
      signal.throwIfAborted();
      const clip = clips.get(clipId);
      const stage = `Preparing source ${index + 1} of ${preparation.sources.length} · ${clip?.name ?? 'Media'}`;
      update({ stage, fraction: null });
      const blob = clip?.source
        ? await fetch(clip.source, { signal }).then((response) => {
          if (!response.ok) throw new Error('The sample media could not be opened.');
          return response.blob();
        })
        : await readMedia(clipId, signal);
      signal.throwIfAborted();
      if (!blob?.size) throw new Error(`Source unavailable · ${clip?.name ?? clipId}. Restore its media before exporting.`);
      await invoke('export_source_begin', { id, clipId, bytes: blob.size });
      for (let offset = 0; offset < blob.size; offset += chunkBytes) {
        signal.throwIfAborted();
        const bytes = await blob.slice(offset, offset + chunkBytes).arrayBuffer();
        signal.throwIfAborted();
        await invoke('export_source_push', bytes, { headers: { 'x-export-id': id } });
        if (offset % (16 * chunkBytes) === 0) update({ stage, fraction: null });
      }
      signal.throwIfAborted();
      await invoke('export_source_finish', { id });
    }
    signal.throwIfAborted();
    update({ stage: 'Checking source media', fraction: 0 });
    const path = await invoke<string>('export_finish', { id });
    complete = true;
    return path;
  } finally {
    unlisten();
    signal.removeEventListener('abort', cancel);
    if (!complete) cancel();
    await cancelling;
  }
}
