import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type RefObject,
} from 'react';
import { timecode } from './types';

const END_PADDING = 24;
const ZOOM_MAX = 100;
const LABEL_SPACING = 84;
const FRAME_INTERVALS = [
  1, 2, 4, 6, 12, 24, 48, 120, 240, 360, 720, 1440, 2880, 7200, 14400,
  28800, 43200, 86400,
];

export function useTimelineZoom(total: number, playhead: number) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [viewport, setViewport] = useState(0);
  const [zoom, setZoom] = useState(0);
  const anchor = useRef<{ time: number; x: number } | null>(null);
  const fit = Math.max(1, viewport - END_PADDING) / (total || 30);
  const maximum = Math.max(240, fit * 4);
  // Exponential scaling gives each step the same relative change in detail.
  const scale = fit * (maximum / fit) ** (zoom / ZOOM_MAX);
  const width =
    zoom === 0
      ? viewport
      : Math.max(viewport, Math.ceil(total * scale + END_PADDING));

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    const measure = () => setViewport(element.clientWidth);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element) {
      if (zoom === 0) element.scrollLeft = 0;
      else if (anchor.current)
        element.scrollLeft = anchor.current.time * scale - anchor.current.x;
    }
    anchor.current = null;
  }, [scale, width, zoom]);

  function changeZoom(value: number) {
    const next = Math.min(ZOOM_MAX, Math.max(0, value));
    const element = scrollRef.current;
    if (!element || next === zoom) return;
    const playheadX = playhead * scale - element.scrollLeft;
    const x = playheadX >= 0 && playheadX < viewport ? playheadX : viewport / 2;
    anchor.current = { time: (element.scrollLeft + x) / scale, x };
    setZoom(next);
  }

  return { scrollRef, viewport, zoom, scale, width, changeZoom };
}

export function TimelineRuler({
  scrollRef,
  viewport,
  scale,
  width,
}: {
  scrollRef: RefObject<HTMLDivElement | null>;
  viewport: number;
  scale: number;
  width: number;
}) {
  const [scrollLeft, setScrollLeft] = useState(0);
  useEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    const sync = () => setScrollLeft(element.scrollLeft);
    sync();
    element.addEventListener('scroll', sync, { passive: true });
    return () => element.removeEventListener('scroll', sync);
  }, [scrollRef]);

  const minimumFrames = (LABEL_SPACING / scale) * 24;
  const frames =
    FRAME_INTERVALS.find((interval) => interval >= minimumFrames) ??
    86400 * Math.ceil(minimumFrames / 86400);
  const spacing = (frames / 24) * scale;
  const offset = Math.min(scrollLeft, Math.max(0, width - viewport));
  const start = Math.max(0, Math.floor(offset / spacing) - 1);
  const end = Math.min(
    Math.floor(width / spacing),
    Math.ceil((offset + viewport) / spacing) + 1,
  );
  const subdivisions = [5, 4, 3, 2].find((count) => frames % count === 0) ?? 1;
  return (
    <div
      className="timeline-ruler"
      style={{ backgroundSize: `${spacing / subdivisions}px 100%` }}
    >
      {Array.from({ length: end - start + 1 }, (_, index) => {
        const tick = start + index;
        return (
          <span key={tick} style={{ left: tick * spacing }}>
            {timecode((tick * frames) / 24)}
          </span>
        );
      })}
    </div>
  );
}
