export type Clip = {
  id: string;
  name: string;
  kind: 'video' | 'audio';
  duration: number;
  addedAt: number;
  size: number;
  source?: string;
  filePath?: string;
  thumbnail?: string;
  width?: number;
  height?: number;
  hasAudio?: boolean;
};

export type Segment = { id: string; clipId: string; in: number; out: number };
export type Message = { id: string; role: 'user' | 'assistant'; text: string };
export type Project = {
  id: string;
  name: string;
  description: string;
  createdAt: number;
  updatedAt: number;
  demo?: boolean;
  clips: Clip[];
  timeline: Segment[];
  messages: Message[];
};

export const uid = () => crypto.randomUUID();
export const segmentLength = (segment: Segment) => segment.out - segment.in;
export const sequenceLength = (segments: Segment[]) =>
  segments.reduce((sum, segment) => sum + segmentLength(segment), 0);
export function timecode(seconds: number) {
  const frames = Math.max(0, Math.round(seconds * 24));
  return [
    Math.floor(frames / 86400),
    Math.floor(frames / 1440) % 60,
    Math.floor(frames / 24) % 60,
    frames % 24,
  ]
    .map((value) => String(value).padStart(2, '0'))
    .join(':');
}
export function shortTime(seconds: number) {
  return `${Math.floor(seconds / 60)}:${String(Math.floor(seconds % 60)).padStart(2, '0')}`;
}

export function splitSegment(
  segment: Segment,
  sourceTime: number,
): [Segment, Segment] {
  return [
    { ...segment, out: sourceTime },
    { ...segment, id: uid(), in: sourceTime },
  ];
}

export function insertSegment(
  timeline: Segment[],
  segment: Segment,
  position: number,
): Segment[] {
  const result: Segment[] = [];
  let cursor = 0;
  let inserted = false;
  for (const existing of timeline) {
    const end = cursor + segmentLength(existing);
    if (!inserted && position <= cursor + 0.01) {
      result.push(segment);
      inserted = true;
    } else if (!inserted && position < end - 0.01) {
      const offset = position - cursor;
      const [left, right] = splitSegment(existing, existing.in + offset);
      result.push(left, segment, right);
      inserted = true;
      cursor = end;
      continue;
    }
    result.push(existing);
    cursor = end;
  }
  if (!inserted) result.push(segment);
  return result;
}
