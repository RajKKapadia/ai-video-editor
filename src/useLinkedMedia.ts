import { invoke } from '@tauri-apps/api/core';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { Clip } from './types';

type MediaFile = { name: string; size: number; url: string };

export function inspectMediaPaths(paths: string[]) {
  return invoke<Record<string, MediaFile>>('inspect_media_paths', { paths });
}

export function useLinkedMedia(clips: Clip[], activePath?: string) {
  const paths = useMemo(
    () => [...new Set(clips.flatMap((clip) => clip.filePath ? [clip.filePath] : []))],
    [clips],
  );
  const [urls, setUrls] = useState<ReadonlyMap<string, string | null>>(new Map());
  const checkCurrent = useRef<() => void>(() => {});
  const refresh = useCallback(() => checkCurrent.current(), []);

  useEffect(() => {
    if (!paths.length) return;
    let active = true;
    let checking = false;
    async function check() {
      if (checking) return;
      checking = true;
      try {
        const files = await inspectMediaPaths(paths);
        if (!active) return;
        const next = new Map(paths.map((path) => [path, files[path]?.url ?? null]));
        setUrls((previous) =>
          previous.size === next.size && paths.every((path) => previous.get(path) === next.get(path))
            ? previous
            : next,
        );
      } catch {
        // A failed check can be retried on focus or during the next poll.
      } finally {
        checking = false;
      }
    }
    checkCurrent.current = check;
    void check();
    const timer = window.setInterval(check, 5000);
    window.addEventListener('focus', check);
    return () => {
      active = false;
      checkCurrent.current = () => {};
      window.clearInterval(timer);
      window.removeEventListener('focus', check);
    };
  }, [paths, activePath]);

  return { urls, refresh };
}
