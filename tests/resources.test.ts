import { describe, expect, test } from 'bun:test';
import {
  importConcurrency,
  mapConcurrent,
  PARALLEL_IMPORT_BYTES,
} from '../src/resources';

const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe('bounded import scheduling', () => {
  test('uses two sessions only with sufficient hardware information and small files', () => {
    const small = [{ size: 1000 }];
    expect(importConcurrency(small, 20, 8)).toBe(2);
    expect(importConcurrency(small, 2, 8)).toBe(1);
    expect(importConcurrency(small, 20, 2)).toBe(1);
    expect(importConcurrency(small, 20)).toBe(1);
    expect(
      importConcurrency([{ size: PARALLEL_IMPORT_BYTES + 1 }], 20, 8),
    ).toBe(1);
  });

  test('bounds simultaneous work, preserves order and continues after a bad file', async () => {
    let running = 0;
    let peak = 0;
    const results = await mapConcurrent([30, 1, 10, 2], 2, async (ms) => {
      peak = Math.max(peak, ++running);
      await delay(ms);
      running--;
      if (ms === 10) throw new Error('unsupported');
      return ms;
    });
    expect(peak).toBe(2);
    expect(running).toBe(0);
    expect(
      results.map((r) => (r.status === 'fulfilled' ? r.value : 'failed')),
    ).toEqual([30, 1, 'failed', 2]);
  });

  test('cancellation stops queued jobs and waits for active jobs to settle', async () => {
    const controller = new AbortController();
    const started: number[] = [];
    const results = await mapConcurrent(
      [1, 2, 3, 4],
      2,
      async (value) => {
        started.push(value);
        await delay(5);
        controller.abort();
        return value;
      },
      controller.signal,
    );
    expect(started).toEqual([1, 2]);
    expect(results).toHaveLength(2);
    expect(
      await mapConcurrent([1], 2, async (x) => x, controller.signal),
    ).toEqual([]);
  });
});
