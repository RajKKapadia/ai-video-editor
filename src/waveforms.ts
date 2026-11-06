import { invoke, isTauri } from '@tauri-apps/api/core';
import { readCacheEntry, readMedia, writeCacheEntry } from './cache';
import type { Clip } from './types';
import { inspectMediaPaths } from './useLinkedMedia';

const RATE = 8000;
const BROWSER_BYTES = 32 * 1024 * 1024;
const BROWSER_SECONDS = 8 * 60;
const CHUNK_BYTES = 1024 * 1024;

export type Waveform = { secondsPerBin: number; peaks: Uint8Array };
export type WaveformEntry = Waveform | { error: string };

let decoding: Promise<unknown> = Promise.resolve();

export async function loadWaveform(clip: Clip, signal: AbortSignal) {
  const key = `waveform.v1:${clip.id}`;
  const cached = await readCacheEntry<Waveform>(key, signal).catch(() => undefined);
  signal.throwIfAborted();
  if (cached) return cached;
  // Serialize decoders even when switching projects during analysis.
  const work = decoding.then(async () => {
    signal.throwIfAborted();
    const readSource = async () => {
      const original = clip.filePath
        ? (await inspectMediaPaths([clip.filePath]))[clip.filePath]
        : undefined;
      if (original && !canDecodeInBrowser(original, clip.duration)) return undefined;
      const url = original?.url ?? clip.source;
      if (!url) return clip.filePath ? undefined : readMedia(clip.id, signal);
      const response = await fetch(url, { signal });
      if (!response.ok) throw new Error('Original media is unavailable.');
      if (original && Number(response.headers.get('content-length')) > BROWSER_BYTES) {
        await response.body?.cancel();
        return undefined;
      }
      return response.blob();
    };
    const source = clip.filePath
      ? { filePath: clip.filePath, size: clip.size }
      : await readSource();
    if (!source) throw new Error('Original media is unavailable.');
    signal.throwIfAborted();
    let waveform: Waveform;
    if (isTauri()) {
      try {
        waveform = await nativeWaveform(source, clip.duration, signal);
      } catch (error) {
        signal.throwIfAborted();
        if (!canDecodeInBrowser(source, clip.duration)) throw error;
        const blob = source instanceof Blob ? source : await readSource();
        if (!blob || !canDecodeInBrowser(blob, clip.duration)) throw error;
        waveform = await browserWaveform(blob, clip.duration, signal);
      }
    } else {
      if (!(source instanceof Blob) || !canDecodeInBrowser(source, clip.duration))
        throw new Error('Open this project in the desktop app to analyze large clips.');
      waveform = await browserWaveform(source, clip.duration, signal);
    }
    signal.throwIfAborted();
    // A full cache should not prevent showing this waveform in the current session.
    await writeCacheEntry(key, waveform).catch(() => {});
    return waveform;
  });
  decoding = work.catch(() => {});
  return work;
}

function canDecodeInBrowser(source: { size: number }, duration: number) {
  return source.size <= BROWSER_BYTES && duration <= BROWSER_SECONDS;
}

async function nativeWaveform(source: Blob | { filePath: string; size: number }, duration: number, signal: AbortSignal) {
  const id = crypto.randomUUID();
  let cancellation: Promise<unknown> | undefined;
  const cancel = () => {
    cancellation ??= invoke('waveform_cancel', { id }).catch(() => {});
    return cancellation;
  };
  await invoke('waveform_begin', {
    id,
    bytes: source.size,
    filePath: source instanceof Blob ? undefined : source.filePath,
  });
  signal.addEventListener('abort', cancel, { once: true });
  try {
    if (source instanceof Blob) {
      for (let offset = 0; offset < source.size; offset += CHUNK_BYTES) {
        signal.throwIfAborted();
        const bytes = await source.slice(offset, offset + CHUNK_BYTES).arrayBuffer();
        signal.throwIfAborted();
        await invoke('waveform_push', bytes, { headers: { 'x-waveform-id': id } });
      }
    }
    signal.throwIfAborted();
    const result = await invoke<ArrayBuffer>('waveform_finish', { id, duration });
    const secondsPerBin = new DataView(result).getUint32(0, true) / RATE;
    const peaks = new Uint8Array(result, 4);
    if (!secondsPerBin || !peaks.length) throw new Error('No waveform data was returned.');
    return { secondsPerBin, peaks };
  } catch (error) {
    await cancel();
    throw error;
  } finally {
    signal.removeEventListener('abort', cancel);
    await cancellation;
  }
}

async function browserWaveform(source: Blob, duration: number, signal: AbortSignal) {
  const context = new OfflineAudioContext(1, 1, RATE);
  const bytes = await source.arrayBuffer();
  signal.throwIfAborted();
  const audio = await context.decodeAudioData(bytes);
  signal.throwIfAborted();
  const binFrames = Math.round(audio.sampleRate / 50);
  const secondsPerBin = binFrames / audio.sampleRate;
  const peaks = new Uint8Array(Math.ceil(duration / secondsPerBin));
  const channels = Array.from({ length: audio.numberOfChannels }, (_, index) =>
    audio.getChannelData(index),
  );
  for (let bin = 0; bin < peaks.length; bin++) {
    if (bin > 0 && bin % 1024 === 0) {
      await new Promise((resolve) => setTimeout(resolve, 0));
      signal.throwIfAborted();
    }
    const start = bin * binFrames;
    const end = Math.min(start + binFrames, audio.length);
    let peak = 0;
    for (const channel of channels) {
      for (let frame = start; frame < end; frame++) {
        peak = Math.max(peak, Math.abs(channel[frame]));
      }
    }
    peaks[bin] = Math.round(Math.sqrt(Math.min(1, peak)) * 255);
  }
  return { secondsPerBin, peaks };
}
