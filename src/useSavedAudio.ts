import { useEffect, useMemo, useRef } from 'react';
import { inspectSavedVideoAudio, readMedia } from './cache';
import type { Clip, Project, Segment } from './types';

export function useSavedAudio(
  timeline: Segment[],
  clipById: ReadonlyMap<string, Clip>,
  importing: boolean,
  linkedUrls: ReadonlyMap<string, string | null>,
  onUpdate: (patch: (project: Project) => Project, metadataOnly?: boolean) => void,
) {
  const attempted = useRef(new Set<string>());
  const pending = useMemo(
    () =>
      [...new Set(timeline.map((segment) => segment.clipId))]
        .map((id) => clipById.get(id))
        .filter((clip) => clip?.kind === 'video' && clip.hasAudio === undefined),
    [timeline, clipById],
  );

  useEffect(() => {
    if (importing || !pending.length) return;
    const controller = new AbortController();
    const { signal } = controller;
    // Only inspect old sources that are in the sequence, once per source and
    // one at a time. Imports already save this information.
    void (async () => {
      const detected = new Map<string, boolean>();
      for (const clip of pending) {
        if (signal.aborted) return;
        if (!clip || attempted.current.has(clip.id)) continue;
        if (clip.filePath && !linkedUrls.get(clip.filePath)) continue;
        try {
          const source = clip.filePath
            ? linkedUrls.get(clip.filePath)
            : clip.source || (await readMedia(clip.id, signal));
          if (source) {
            const hasAudio = await inspectSavedVideoAudio(source, signal, !!clip.filePath);
            if (hasAudio !== undefined) detected.set(clip.id, hasAudio);
          }
        } catch {
          // A missing or unreadable original can still remain on the timeline.
        }
        if (!signal.aborted && !detected.has(clip.id))
          attempted.current.add(clip.id);
      }
      if (!signal.aborted && detected.size) {
        onUpdate(
          (project) => ({
            ...project,
            clips: project.clips.map((clip) => {
              const hasAudio = detected.get(clip.id);
              return clip.hasAudio === undefined && hasAudio !== undefined
                ? { ...clip, hasAudio }
                : clip;
            }),
          }),
          true,
        );
      }
    })();
    return () => controller.abort();
  }, [pending, importing, linkedUrls, onUpdate]);
}
