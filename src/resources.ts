// Decoder sessions and media buffers are scarcer than CPU cores. These are
// conservative scheduling limits, not promises about process RAM or temperature.
export const PREFETCH_BYTES = 256 * 1024 * 1024;
export const PARALLEL_IMPORT_BYTES = 128 * 1024 * 1024;

export function importConcurrency(
  files: readonly { size: number }[],
  cores: number,
  memoryGiB?: number,
) {
  return cores >= 4 &&
    memoryGiB !== undefined &&
    memoryGiB >= 4 &&
    files.every((file) => file.size <= PARALLEL_IMPORT_BYTES)
    ? 2
    : 1;
}

// Preserve the user's file order and isolate failures without starting an
// unbounded Promise.all of decoders or storage transactions.
export async function mapConcurrent<T, R>(
  items: readonly T[],
  concurrency: number,
  task: (item: T) => Promise<R>,
  signal?: AbortSignal,
): Promise<PromiseSettledResult<R>[]> {
  const results: PromiseSettledResult<R>[] = [];
  let next = 0;
  const count = Math.max(
    1,
    Math.min(items.length, Math.floor(concurrency) || 1),
  );
  await Promise.all(
    Array.from({ length: count }, async () => {
      while (next < items.length && !signal?.aborted) {
        const index = next++;
        try {
          results[index] = {
            status: 'fulfilled',
            value: await task(items[index]),
          };
        } catch (reason) {
          results[index] = { status: 'rejected', reason };
        }
      }
    }),
  );
  return results;
}
