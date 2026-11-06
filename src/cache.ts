import { type Project, type Clip, uid } from './types';

const CACHE_KEY = 'cutline.projects.v1';
function sampleProject(): Project {
  const now = Date.now();
  const clips: Clip[] = [
    {
      id: 'demo-coast',
      name: '01_Ocean at first light.mp4',
      kind: 'video',
      duration: 12,
      source: '/demo/coast.mp4',
      thumbnail: '/demo/coast.jpg',
      addedAt: now - 3000,
      size: 0,
      width: 960,
      height: 540,
      hasAudio: false,
    },
    {
      id: 'demo-forest',
      name: '02_Through the trees.mp4',
      kind: 'video',
      duration: 10,
      source: '/demo/forest.mp4',
      thumbnail: '/demo/forest.jpg',
      addedAt: now - 2000,
      size: 0,
      width: 960,
      height: 540,
      hasAudio: false,
    },
    {
      id: 'demo-lake',
      name: '03_A moment of stillness.mp4',
      kind: 'video',
      duration: 8,
      source: '/demo/lake.mp4',
      thumbnail: '/demo/lake.jpg',
      addedAt: now - 1000,
      size: 0,
      width: 960,
      height: 540,
      hasAudio: false,
    },
  ];
  return {
    id: 'demo-project',
    name: 'Somewhere, slowly',
    description:
      'A small escape. Ocean air, forest trails, and a moment to slow down.',
    createdAt: now,
    updatedAt: now,
    demo: true,
    clips,
    timeline: [
      { id: 'demo-segment-1', clipId: clips[0].id, in: 1, out: 7 },
      { id: 'demo-segment-2', clipId: clips[1].id, in: 0, out: 5 },
      { id: 'demo-segment-3', clipId: clips[2].id, in: 1, out: 6 },
    ],
    messages: [],
  };
}

export function validProject(value: unknown): value is Project {
  if (!value || typeof value !== 'object') return false;
  const p = value as Project;
  return (
    typeof p.id === 'string' &&
    typeof p.name === 'string' &&
    typeof p.description === 'string' &&
    Number.isFinite(p.createdAt) &&
    Number.isFinite(p.updatedAt) &&
    Array.isArray(p.clips) &&
    p.clips.every(
      (c) =>
        typeof c.id === 'string' &&
        typeof c.name === 'string' &&
        ['video', 'audio'].includes(c.kind) &&
          (c.filePath === undefined || typeof c.filePath === 'string') &&
        Number.isFinite(c.duration) &&
        c.duration > 0,
    ) &&
    Array.isArray(p.timeline) &&
    p.timeline.every(
      (s) =>
        typeof s.id === 'string' &&
        p.clips.some(
          (c) =>
            c.id === s.clipId &&
            s.in >= 0 &&
            s.out > s.in &&
            s.out <= c.duration + 0.1,
        ),
    ) &&
    Array.isArray(p.messages) &&
    p.messages.every(
      (m) =>
        typeof m.text === 'string' && ['user', 'assistant'].includes(m.role),
    )
  );
}

export function loadProjects(): { projects: Project[]; error: string | null } {
  try {
    const stored = localStorage.getItem(CACHE_KEY);
    if (!stored) return { projects: [sampleProject()], error: null };
    const parsed: unknown = JSON.parse(stored);
    if (!Array.isArray(parsed) || !parsed.every(validProject))
      throw new Error('Invalid project cache');
    return { projects: parsed, error: null };
  } catch {
    return {
      projects: [],
      error:
        'Your saved projects could not be read. Changes will stay in this session.',
    };
  }
}
export function saveProjects(projects: Project[]) {
  localStorage.setItem(CACHE_KEY, JSON.stringify(projects));
}

export function legacyProjects() {
  return localStorage.getItem(CACHE_KEY) === null ? null : loadProjects();
}

export function clearLegacyProjects() {
  localStorage.removeItem(CACHE_KEY);
}
export function createProject(name: string, description: string): Project {
  return {
    id: uid(),
    name: name.trim(),
    description: description.trim(),
    createdAt: Date.now(),
    updatedAt: Date.now(),
    clips: [],
    timeline: [],
    messages: [],
  };
}

let database: Promise<IDBDatabase> | undefined;
function openDatabase(): Promise<IDBDatabase> {
  database ??= new Promise((resolve, reject) => {
    const request = indexedDB.open('cutline-media', 1);
    request.onupgradeneeded = () => request.result.createObjectStore('files');
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => {
      database = undefined;
      reject(request.error);
    };
  });
  return database;
}
export async function writeCacheEntry(id: string, value: unknown) {
  const db = await openDatabase();
  return new Promise<void>((resolve, reject) => {
    const tx = db.transaction('files', 'readwrite');
    tx.objectStore('files').put(value, id);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}
export function storeMedia(id: string, file: Blob) {
  return writeCacheEntry(id, file);
}
export async function readCacheEntry<T>(
  id: string,
  signal?: AbortSignal,
): Promise<T | undefined> {
  const db = await openDatabase();
  signal?.throwIfAborted();
  return new Promise((resolve, reject) => {
    const tx = db.transaction('files', 'readonly');
    const abort = () => tx.abort();
    signal?.addEventListener('abort', abort, { once: true });
    const cleanup = () => signal?.removeEventListener('abort', abort);
    tx.oncomplete = cleanup;
    tx.onabort = () => {
      cleanup();
      reject(signal?.reason ?? tx.error ?? new Error('Media read cancelled'));
    };
    const request = tx.objectStore('files').get(id);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}
export function readMedia(id: string, signal?: AbortSignal) {
  return readCacheEntry<Blob>(id, signal);
}

function loadMedia(
  media: HTMLMediaElement,
  url: string,
  firstFrame: boolean,
  signal?: AbortSignal,
) {
  signal?.throwIfAborted();
  return new Promise<void>((resolve, reject) => {
    const finish = (error?: Error) => {
      clearTimeout(timeout);
      media.onloadeddata = null;
      media.onloadedmetadata = null;
      media.onerror = null;
      signal?.removeEventListener('abort', abort);
      if (error) reject(error);
      else resolve();
    };
    const abort = () =>
      finish(new DOMException('Media inspection cancelled', 'AbortError'));
    const timeout = window.setTimeout(
      () => finish(new Error('Could not read media')),
      15000,
    );
    if (firstFrame) media.onloadeddata = () => finish();
    else media.onloadedmetadata = () => finish();
    media.onerror = () => finish(new Error('Unsupported media'));
    signal?.addEventListener('abort', abort, { once: true });
    media.src = url;
    media.load();
  });
}

function releaseMedia(media: HTMLMediaElement, objectUrl?: string) {
  media.pause();
  media.removeAttribute('src');
  media.load();
  if (objectUrl) URL.revokeObjectURL(objectUrl);
}

function embeddedAudio(video: HTMLVideoElement): boolean | undefined {
  const media = video as HTMLVideoElement & {
    audioTracks?: { length: number };
    mozHasAudio?: boolean;
    webkitAudioDecodedByteCount?: number;
    captureStream?: () => MediaStream;
  };
  if (media.audioTracks) return media.audioTracks.length > 0;
  if (typeof media.mozHasAudio === 'boolean') return media.mozHasAudio;
  if (media.webkitAudioDecodedByteCount) return true;
  // Chromium exposes its selected media tracks through captureStream.
  // Inspect the track list and immediately stop the capture; no playback needed.
  try {
    const tracks = media.captureStream?.().getTracks();
    if (tracks) {
      tracks.forEach((track) => track.stop());
      if (tracks.some((track) => track.kind === 'audio')) return true;
      if (tracks.some((track) => track.kind === 'video')) return false;
    }
  } catch {
    // Some engines cannot capture a media element. Leave the status unknown.
  }
}

export async function inspectSavedVideoAudio(
  source: Blob | string,
  signal: AbortSignal,
  crossOrigin = false,
) {
  signal.throwIfAborted();
  const media = document.createElement('video');
  if (crossOrigin) media.crossOrigin = 'anonymous';
  const fromUrl = typeof source === 'string';
  const url = fromUrl ? source : URL.createObjectURL(source);
  media.preload = 'metadata';
  media.muted = true;
  try {
    await loadMedia(media, url, false, signal);
    return embeddedAudio(media);
  } finally {
    releaseMedia(media, fromUrl ? undefined : url);
  }
}

export async function inspectMedia(
  file: File | { name: string; size: number; filePath: string; url: string },
  signal?: AbortSignal,
): Promise<Clip> {
  signal?.throwIfAborted();
  const linked = 'filePath' in file;
  const kind =
    (!linked && file.type.startsWith('audio/')) ||
    /\.(mp3|wav|m4a|aac|ogg|oga|opus|flac|aif|aiff)$/i.test(file.name)
      ? 'audio'
      : 'video';
  const media = document.createElement(kind);
  const url = linked ? file.url : URL.createObjectURL(file);
  if (linked) media.crossOrigin = 'anonymous';
  media.preload = kind === 'audio' ? 'metadata' : 'auto';
  media.muted = true;
  try {
    await loadMedia(media, url, kind === 'video', signal);
    if (!Number.isFinite(media.duration) || media.duration <= 0)
      throw new Error('The clip has no readable duration');
    let thumbnail: string | undefined;
    let width: number | undefined;
    let height: number | undefined;
    let hasAudio: boolean | undefined;
    if (media instanceof HTMLVideoElement) {
      hasAudio = embeddedAudio(media);
      width = media.videoWidth;
      height = media.videoHeight;
      const canvas = document.createElement('canvas');
      canvas.width = 320;
      canvas.height = 180;
      const context = canvas.getContext('2d');
      if (context && width > 0 && height > 0) {
        const scale = Math.max(320 / width, 180 / height);
        context.drawImage(
          media,
          (320 - width * scale) / 2,
          (180 - height * scale) / 2,
          width * scale,
          height * scale,
        );
        // Let the browser encode asynchronously instead of blocking editing
        // with synchronous JPEG/base64 conversion for each imported clip.
        const blob = await new Promise<Blob | null>((resolve) =>
          canvas.toBlob(resolve, 'image/jpeg', 0.65),
        );
        signal?.throwIfAborted();
        if (blob)
          thumbnail = await new Promise<string>((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = () => resolve(reader.result as string);
            reader.onerror = () => reject(reader.error);
            reader.readAsDataURL(blob);
          });
      }
      canvas.width = canvas.height = 0;
    }
    signal?.throwIfAborted();
    return {
      id: uid(),
      name: file.name,
      kind,
      duration: media.duration,
      size: file.size,
      filePath: linked ? file.filePath : undefined,
      addedAt: Date.now(),
      thumbnail,
      width,
      height,
      hasAudio,
    };
  } finally {
    releaseMedia(media, linked ? undefined : url);
  }
}
