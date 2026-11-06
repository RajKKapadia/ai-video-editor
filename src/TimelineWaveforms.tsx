import { memo, useEffect, useRef, type RefObject } from 'react';
import { segmentLength, type Segment } from './types';
import type { Waveform, WaveformEntry } from './waveforms';

type PositionedSegment = Segment & { start: number };
const BLOCK = 64;
const summaries = new WeakMap<Uint8Array, Uint8Array>();

function peakAt(waveform: Waveform, from: number, to: number) {
  const { peaks, secondsPerBin } = waveform;
  let start = Math.max(0, Math.floor(from / secondsPerBin));
  const end = Math.min(peaks.length, Math.ceil(to / secondsPerBin));
  let peak = 0;
  // Coarse zoom combines every covered bin, so short sounds don't disappear.
  if (end - start > BLOCK) {
    let summary = summaries.get(peaks);
    if (!summary) {
      summary = new Uint8Array(Math.ceil(peaks.length / BLOCK));
      for (let i = 0; i < peaks.length; i++) {
        const block = Math.floor(i / BLOCK);
        summary[block] = Math.max(summary[block], peaks[i]);
      }
      summaries.set(peaks, summary);
    }
    for (; start % BLOCK !== 0 && start < end; start++)
      peak = Math.max(peak, peaks[start]);
    for (; start + BLOCK <= end; start += BLOCK)
      peak = Math.max(peak, summary[start / BLOCK]);
  }
  for (; start < end; start++) peak = Math.max(peak, peaks[start]);
  return peak;
}

export const TimelineWaveforms = memo(function TimelineWaveforms({
  scrollRef,
  viewport,
  scale,
  segments,
  entries,
}: {
  scrollRef: RefObject<HTMLDivElement | null>;
  viewport: number;
  scale: number;
  segments: PositionedSegment[];
  entries: ReadonlyMap<string, WaveformEntry>;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const scroll = scrollRef.current;
    const canvas = canvasRef.current;
    const context = canvas?.getContext('2d');
    if (!scroll || !canvas || !context) return;

    let frame: number | undefined;
    const draw = () => {
      frame = undefined;
      const width = canvas.clientWidth;
      const height = canvas.clientHeight;
      if (!width || !height) return;
      const density = Math.min(window.devicePixelRatio || 1, 2);
      const pixelWidth = Math.round(width * density);
      const pixelHeight = Math.round(height * density);
      if (canvas.width !== pixelWidth) canvas.width = pixelWidth;
      if (canvas.height !== pixelHeight) canvas.height = pixelHeight;
      context.setTransform(density, 0, 0, density, 0, 0);
      context.clearRect(0, 0, width, height);
      const offset = scroll.scrollLeft;
      const middle = height / 2;
      for (const segment of segments) {
        const left = segment.start * scale - offset;
        if (left >= width) break;
        const right = left + segmentLength(segment) * scale;
        if (right <= 0) continue;
        const entry = entries.get(segment.clipId);
        if (!entry || 'error' in entry) continue;

        const visibleLeft = Math.max(0, left + 1);
        const visibleRight = Math.min(width, right - 1);
        if (visibleRight <= visibleLeft) continue;
        context.fillStyle = '#6f8f78';
        context.fillRect(visibleLeft, middle - 0.5, visibleRight - visibleLeft, 1);
        context.fillStyle = '#a5d1ae';
        for (let x = Math.floor(visibleLeft); x < visibleRight; x++) {
          const start = Math.max(x, visibleLeft);
          const end = Math.min(x + 1, visibleRight);
          const from = segment.in + (start - left) / scale;
          const to = Math.min(segment.out, segment.in + (end - left) / scale);
          const peak = peakAt(entry, from, to);
          if (peak === 0) continue;
          const half = Math.max(0.75, (peak / 255) * (middle - 1));
          context.fillRect(start, middle - half, end - start, half * 2);
        }
      }
    };
    const schedule = () => {
      if (frame === undefined) frame = requestAnimationFrame(draw);
    };
    scroll.addEventListener('scroll', schedule, { passive: true });
    const observer = new ResizeObserver(schedule);
    observer.observe(canvas);
    schedule();
    return () => {
      if (frame !== undefined) cancelAnimationFrame(frame);
      observer.disconnect();
      scroll.removeEventListener('scroll', schedule);
    };
  }, [scrollRef, viewport, scale, segments, entries]);

  return (
    <canvas
      ref={canvasRef}
      className="timeline-waveforms"
      style={{ width: viewport }}
      aria-hidden="true"
    />
  );
});
