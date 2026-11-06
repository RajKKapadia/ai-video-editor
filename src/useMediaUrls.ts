import { useEffect, useRef, useState } from 'react';
import { readMedia } from './cache';
import { PREFETCH_BYTES } from './resources';
import type { Clip } from './types';

export function useMediaUrls(active?: Clip, next?: Clip) {
  const cache = useRef(new Map<string, string>());
  const [urls, setUrls] = useState<Record<string, string>>({});
  const [failedId, setFailedId] = useState<string>();
  const activeId = active && !active.source && !active.filePath ? active.id : undefined;
  // One optional look-ahead handle; never preload an entire project. Large
  // sources still play, but suppress speculation when the byte budget is exceeded.
  const nextId =
    next &&
    !next.source &&
    !next.filePath &&
    next.id !== activeId &&
    (active?.size ?? 0) + next.size <= PREFETCH_BYTES
      ? next.id
      : undefined;

  useEffect(() => {
    const controller = new AbortController();
    const desired = [activeId, nextId].filter((id): id is string => !!id);
    const entries = cache.current;
    for (const [id, url] of entries) {
      if (!desired.includes(id)) {
        URL.revokeObjectURL(url);
        entries.delete(id);
      }
    }
    setUrls(Object.fromEntries(entries));
    setFailedId(undefined);
    void (async () => {
      // The active clip gets first access to disk; the next clip only follows it.
      for (const id of desired) {
        if (controller.signal.aborted) return;
        if (entries.has(id)) continue;
        try {
          const blob = await readMedia(id, controller.signal);
          if (controller.signal.aborted) return;
          if (!blob) throw new Error('Media missing');
          entries.set(id, URL.createObjectURL(blob));
          setUrls(Object.fromEntries(entries));
        } catch {
          if (controller.signal.aborted) return;
          if (id === activeId) setFailedId(id);
        }
      }
    })();
    return () => controller.abort();
  }, [activeId, nextId]);

  useEffect(() => {
    const entries = cache.current;
    return () => {
      for (const url of entries.values()) URL.revokeObjectURL(url);
      entries.clear();
    };
  }, []);

  return {
    urls,
    loading: !!activeId && !urls[activeId] && failedId !== activeId,
  };
}
