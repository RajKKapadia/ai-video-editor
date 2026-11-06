import { invoke, isTauri } from '@tauri-apps/api/core';
import { useEffect, useRef, useState } from 'react';
import { clearLegacyProjects, legacyProjects, loadProjects, saveProjects, validProject } from './cache';
import { uid, type Project } from './types';

type Snapshot = { revision: string; projects: Project[] };
const readError = 'Your saved projects could not be read. Changes will stay in this session.';
const writeError = 'Changes could not be saved. Your device storage may be full or unavailable.';

async function readSnapshot() {
  const snapshot = await invoke<Snapshot | null>('load_project_cache');
  if (snapshot && !snapshot.projects.every(validProject)) throw new Error(readError);
  return snapshot;
}

async function writeSnapshot(previous: string | null, projects: Project[]) {
  const snapshot = { revision: uid(), projects };
  const saved = await invoke<boolean>('save_project_cache', { previous, snapshot });
  return saved ? snapshot : null;
}

async function openDesktopCache() {
  const existing = await readSnapshot();
  const legacy = legacyProjects();
  if (!legacy) return existing;
  if (legacy.error) {
    if (existing) return existing;
    throw new Error(legacy.error);
  }
  const ids = new Set(existing?.projects.map((project) => project.id));
  const additions = legacy.projects.filter((project) => !ids.has(project.id));
  // A successful write retires the old project list so a cache reset cannot
  // resurrect it. Legacy media remains accessible through its existing clip IDs.
  const snapshot = existing && !additions.length
    ? existing
    : await writeSnapshot(existing?.revision ?? null, [...(existing?.projects ?? []), ...additions]);
  if (snapshot) clearLegacyProjects();
  return snapshot ?? readSnapshot();
}

export function useProjectCache() {
  const desktop = isTauri();
  const [initial] = useState(() => desktop ? null : loadProjects());
  const [projects, setProjects] = useState<Project[]>(initial?.projects ?? []);
  const [cacheError, setCacheError] = useState(initial?.error ?? null);
  const [loading, setLoading] = useState(desktop);
  const blocked = useRef(!!initial?.error);
  const opening = useRef<Promise<Snapshot | null> | null>(null);
  const saved = useRef<Snapshot | null>(null);
  const current = useRef(projects);
  const writes = useRef(Promise.resolve());

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    opening.current ??= openDesktopCache();
    void opening.current.then((snapshot) => {
      if (!active) return;
      saved.current = snapshot;
      setProjects(snapshot?.projects ?? []);
    }).catch(() => {
      if (!active) return;
      blocked.current = true;
      setCacheError(readError);
    }).finally(() => {
      if (active) setLoading(false);
    });
    return () => { active = false; };
  }, [desktop]);

  useEffect(() => {
    if (loading || blocked.current) return;
    current.current = projects;
    if (!desktop) {
      try {
        saveProjects(projects);
        setCacheError(null);
      } catch {
        setCacheError(writeError);
      }
      return;
    }

    writes.current = writes.current.then(async () => {
      if (projects !== current.current || projects === saved.current?.projects) return;
      if (!projects.length && !saved.current) return;
      const snapshot = await writeSnapshot(saved.current?.revision ?? null, projects);
      if (snapshot) {
        saved.current = snapshot;
      } else {
        // The cache was removed or replaced while an edit was in memory.
        saved.current = await readSnapshot();
        current.current = saved.current?.projects ?? [];
        setProjects(current.current);
      }
      setCacheError(null);
    }).catch(() => setCacheError(writeError));
  }, [desktop, loading, projects]);

  useEffect(() => {
    if (!desktop || loading) return;
    let active = true;
    let checking = false;
    function check() {
      if (!saved.current || checking) return;
      checking = true;
      writes.current = writes.current.then(async () => {
        if (saved.current && !await invoke<boolean>('project_cache_exists') && active) {
          saved.current = null;
          current.current = [];
          setProjects(current.current);
          setCacheError(null);
        }
      }).catch(() => {
        // A failed probe is distinct from a deleted cache.
      }).finally(() => {
        checking = false;
      });
    }
    const timer = window.setInterval(check, 2000);
    window.addEventListener('focus', check);
    return () => {
      active = false;
      window.clearInterval(timer);
      window.removeEventListener('focus', check);
    };
  }, [desktop, loading]);

  return { projects, setProjects, cacheError, loading };
}
