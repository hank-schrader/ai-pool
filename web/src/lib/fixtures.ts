import type { Snapshot } from './protocol';
const epoch = '00000000-0000-4000-8000-000000000001';
export const t = (second: number) => new Date(second * 1000).toISOString().replace('.000Z', 'Z');
export function snapshot(second = 181, seq = 1): Snapshot {
  return { version: 1, epoch, seq, updated_at: t(second),
    totals: { miners: 2, queued_questions: 3, answered_last_24h: 60, answered_per_second: 1 },
    live: [{ t: t(second - 1), answered: 2, queued: 3, miners: 2 }],
    history: { interval_seconds: 60, end: t(120), answered: [60], queued: [3], miners: [2] } };
}
