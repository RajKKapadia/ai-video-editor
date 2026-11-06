import { useEffect, useRef, useState } from 'react';
import type { Clip } from './types';
import { loadWaveform, type WaveformEntry } from './waveforms';

export function useWaveforms(sources: Clip[]) {
  const [entries, setEntries] = useState<ReadonlyMap<string, WaveformEntry>>(
    () => new Map(),
  );
  const done = useRef(new Map<string, WaveformEntry>());
  const desired = useRef<Clip[]>([]);
  const session = useRef<AbortController | null>(null);
  const worker = useRef<AbortController | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    session.current = controller;
    return () => controller.abort();
  }, []);

  useEffect(() => {
    desired.current = sources;
    const controller = session.current;
    if (!controller || worker.current === controller) return;
    worker.current = controller;
    const { signal } = controller;
    // Timeline edits update the queue without restarting a running decode.
    void (async () => {
      try {
        while (!signal.aborted) {
          const source = desired.current.find(
            (clip) => !done.current.has(clip.id),
          );
          if (!source) return;
          let entry: WaveformEntry;
          try {
            entry = await loadWaveform(source, signal);
          } catch (error) {
            entry = {
              error:
                typeof error === 'string'
                  ? error
                  : error instanceof Error
                    ? error.message
                    : 'Could not analyze this audio.',
            };
          }
          if (signal.aborted) return;
          done.current.set(source.id, entry);
          setEntries(new Map(done.current));
        }
      } finally {
        if (worker.current === controller) worker.current = null;
      }
    })();
  }, [sources]);

  return entries;
}
