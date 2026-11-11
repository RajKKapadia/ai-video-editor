import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readMedia } from './cache';
import type { CleanupReport } from './cleanupTimeline';
import { uid, type Clip, type Segment } from './types';

export const providers = {
  openAi: 'OpenAI',
  anthropic: 'Anthropic',
  gemini: 'Google Gemini',
  compatible: 'OpenAI-compatible',
} as const;

export type Provider = keyof typeof providers;
export type ProviderConfig = { model: string; baseUrl: string | null; contextWindowTokens?: number };
export type ProviderProfile = ProviderConfig & { hasKey: boolean; ready: boolean };
export type ProviderStatus = {
  active: Provider | null;
  profiles: Partial<Record<Provider, ProviderProfile>>;
  warning: string | null;
};
export type ProviderSelection = { provider: Provider; config: ProviderConfig };

export type CleanupProgress = { stage: string; fraction: number | null };
export type AssistantStage = 'draft' | 'pauses' | 'cuts' | 'chat';
export type AssistantRequest = { stage: AssistantStage; message: string; provider: ProviderSelection };
export type CleanupOptions = { pauses: boolean; retakes: ProviderSelection | null; assistant?: AssistantRequest };

export const adaptiveCleanupBrief = 'Build a flowing sequence using cuts and joins. Remove clear false starts, accidental repeated words or passages, recording count-ins, and dispensable quiet gaps. Connect partial attempts to their continuations, keeping overlapping words only once. Preserve unique details, necessary connecting words, intentional repetition, natural local rhythm, intentional pauses, and time used for demonstrations.';

const chunkBytes = 1024 * 1024;

export function hasAnalyzableAudio(clip: Clip | undefined) {
  return !!clip && (clip.kind === 'audio' || clip.hasAudio !== false);
}

export function errorMessage(error: unknown, fallback: string) {
  return typeof error === 'string' ? error : error instanceof Error ? error.message : fallback;
}

export async function analyzeTimeline(
  projectId: string,
  timeline: Segment[],
  clips: ReadonlyMap<string, Clip>,
  options: CleanupOptions,
  signal: AbortSignal,
  update: (progress: CleanupProgress) => void,
) {
  const id = uid();
  let cancelling: Promise<unknown> | undefined;
  const cancel = () => {
    cancelling = invoke('cleanup_cancel', { id }).catch(() => {});
  };
  const unlisten = await listen<CleanupProgress & { id: string }>('timeline-cleanup-progress', ({ payload }) => {
    if (payload.id !== id) return;
    if (signal.aborted) cancel();
    else update(payload);
  });
  let complete = false;
  signal.addEventListener('abort', cancel, { once: true });
  try {
    signal.throwIfAborted();
    const sources = await invoke<string[]>('cleanup_begin', {
      id,
      projectId,
      segments: timeline.map((segment) => ({
        clipId: segment.clipId,
        in: segment.in,
        out: segment.out,
        analyze: hasAnalyzableAudio(clips.get(segment.clipId)),
      })),
      ...options,
    });
    for (const [index, clipId] of sources.entries()) {
      signal.throwIfAborted();
      const clip = clips.get(clipId);
      if (clip?.filePath) {
        await invoke('cleanup_source_finish', { id, original: { clipId, filePath: clip.filePath } });
        continue;
      }
      const stage = 'Preparing source ' + (index + 1) + ' of ' + sources.length + ' · ' + (clip?.name ?? 'source media');
      update({ stage, fraction: null });
      const blob = await readMedia(clipId, signal).catch(() => undefined);
      signal.throwIfAborted();
      if (!blob?.size || blob.size > 64 * 1024 ** 3) {
        await invoke('cleanup_source_skip', { id, clipId });
        continue;
      }
      await invoke('cleanup_source_begin', { id, clipId, bytes: blob.size });
      for (let offset = 0; offset < blob.size; offset += chunkBytes) {
        signal.throwIfAborted();
        const bytes = await blob.slice(offset, offset + chunkBytes).arrayBuffer();
        signal.throwIfAborted();
        await invoke('cleanup_source_push', bytes, { headers: { 'x-cleanup-id': id } });
        if (offset % (16 * chunkBytes) === 0) update({ stage, fraction: offset / blob.size });
      }
      signal.throwIfAborted();
      await invoke('cleanup_source_finish', { id });
    }
    signal.throwIfAborted();
    const report = await invoke<CleanupReport>('cleanup_finish', { id });
    signal.throwIfAborted();
    complete = true;
    return report;
  } finally {
    unlisten();
    signal.removeEventListener('abort', cancel);
    if (!complete) cancel();
    await cancelling;
  }
}
