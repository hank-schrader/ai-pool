import { describe, expect, it } from 'vitest';
import { parseRest, parseSnapshot, parseTick, type Snapshot, type Tick } from './protocol';
import { applyTick, historicalSeries, liveSeries, replaceSnapshot, niceScale, tickFormatter } from './stats';
import { snapshot, t } from './fixtures';
function tick(state: Snapshot, answered = 4): Tick {
  return { version: 1, epoch: state.epoch, seq: state.seq + 1, updated_at: t(Date.parse(state.updated_at) / 1000 + 1), totals: state.totals,
    second: { t: state.updated_at, answered, queued: 1, miners: 2 }, minute: null };
}
describe('validated protocol', () => {
  it('accepts snapshots, ticks and the compatible REST shape', () => {
    const s = snapshot();
    expect(parseSnapshot(s)).toEqual(s);
    expect(parseTick(tick(s))).toEqual(tick(s));
    expect(parseRest({ ...s, ...s.totals, totals: undefined })).toEqual(s);
  });
  it('rejects invalid schema, dates, rates, counts, coverage and parallel arrays', () => {
    const s = snapshot();
    const invalid = [null, {}, { ...s, version: 2 }, { ...s, epoch: 'bad' }, { ...s, seq: -1 },
      { ...s, updated_at: '1970-02-31T00:00:00Z' }, { ...s, totals: { ...s.totals, answered_per_second: Infinity } },
      { ...s, totals: { ...s.totals, miners: -1 } }, { ...s, history: { ...s.history, queued: [] } },
      { ...s, live: [{ ...s.live[0], t: s.updated_at }] }, { ...s, live: [s.live[0], s.live[0]] }];
    for (const value of invalid) expect(parseSnapshot(value)).toBeNull();
    expect(parseTick({ ...tick(s), minute: { ...s.live[0], t: t(120) } })).toBeNull();
  });
});
describe('immutable reducer', () => {
  it('applies one tick and ignores duplicates', () => {
    const s = snapshot(), next = tick(s), result = applyTick(s, next);
    expect(result.resync).toBe(false);
    expect(result.state?.live).toHaveLength(2);
    expect(s.live).toHaveLength(1);
    expect(applyTick(result.state, next).changed).toBe(false);
  });
  it('accepts the first full second after an empty startup snapshot', () => {
    const s = { ...snapshot(), live: [] };
    expect(applyTick(s, tick(s)).state?.live).toHaveLength(1);
    const delayed = tick(s);
    delayed.second = { ...delayed.second, t: t(182) };
    delayed.updated_at = t(183);
    expect(applyTick(s, delayed).state?.live).toHaveLength(1);
    delayed.second = { ...delayed.second, t: t(183) };
    delayed.updated_at = t(184);
    expect(applyTick(s, delayed).resync).toBe(true);
  });
  it('requires snapshot replacement after restart, gaps or a skipped second', () => {
    const s = snapshot(), next = tick(s);
    expect(applyTick(null, next).resync).toBe(true);
    expect(applyTick(s, { ...next, epoch: '00000000-0000-4000-8000-000000000002' }).resync).toBe(true);
    expect(applyTick(s, { ...next, seq: next.seq + 1 }).resync).toBe(true);
    expect(applyTick(s, { ...next, second: { ...next.second, t: t(185) } }).resync).toBe(true);
    expect(replaceSnapshot(s, s).changed).toBe(false);
    expect(replaceSnapshot(s, { ...s, seq: 0, epoch: '00000000-0000-4000-8000-000000000002' }).changed).toBe(true);
  });
  it('bounds second and minute history', () => {
    let s = snapshot();
    for (let i = 0; i < 350; i++) s = applyTick(s, tick(s)).state!;
    expect(s.live).toHaveLength(300);
    const full = { ...s, history: { ...s.history, answered: Array(1440).fill(1), queued: Array(1440).fill(0), miners: Array(1440).fill(2), end: t(480) } };
    const next = { ...tick(full), second: { t: t(599), answered: 1, queued: 0, miners: 2 }, minute: { t: t(540), answered: 60, queued: 0, miners: 2 } };
    const initial = { ...full, updated_at: t(599), live: [{ t: t(598), answered: 1, queued: 0, miners: 2 }] };
    expect(applyTick(initial, next).state?.history.answered).toHaveLength(1440);
  });
});
describe('series and provisional minutes', () => {
  it('uses a trailing average of up to ten completed seconds', () => {
    const s = snapshot();
    s.live = Array.from({ length: 12 }, (_, i) => ({ t: t(180 + i), answered: i + 1, queued: 0, miners: 1 }));
    const points = liveSeries(s);
    expect(points[0].average).toBe(1);
    expect(points[9].average).toBe(5.5);
    expect(points[11].average).toBe(7.5);
  });
  it('marks incomplete startup coverage and divides by observed completed seconds', () => {
    const s = snapshot(188);
    s.live = [{ t: t(185), answered: 2, queued: 0, miners: 1 }, { t: t(186), answered: 4, queued: 0, miners: 1 }];
    const p = historicalSeries(s, 60).at(-1)!;
    expect(p).toMatchObject({ t: 180000, v: 3, provisional: true, incomplete: true, seconds: 2 });
  });
  it('replaces so-far rates with authoritative minutes and omits provisional at rollover', () => {
    const s = snapshot(239);
    s.live = Array.from({ length: 59 }, (_, i) => ({ t: t(180 + i), answered: 1, queued: 0, miners: 1 }));
    expect(historicalSeries(s, 60).at(-1)).toMatchObject({ provisional: true, v: 1, incomplete: false });
    const next = tick(s, 1); next.minute = { t: t(180), answered: 60, queued: 0, miners: 1 };
    const rolled = applyTick(s, next).state!;
    expect(historicalSeries(rolled, 60).at(-1)).toEqual({ t: 180000, v: 1 });
    const current = applyTick(rolled, tick(rolled, 2)).state!;
    expect(historicalSeries(current, 60).at(-1)).toMatchObject({ t: 240000, v: 2, seconds: 1 });
  });
});

describe('axis tick precision', () => {
  it('preserves the decimals implied by clean scale steps', () => {
    const scale = niceScale(0);
    expect(scale).toEqual({ step: 0.25, top: 1 });
    const quarter = tickFormatter(scale.step, 'en-US');
    expect([0, 0.25, 0.5, 0.75, 1].map(v => quarter.format(v))).toEqual(['0.00', '0.25', '0.50', '0.75', '1.00']);
    expect(tickFormatter(2.5, 'en-US').format(7.5)).toBe('7.5');
    expect(tickFormatter(2, 'en-US').format(6)).toBe('6');
    expect(tickFormatter(10, 'en-US').format(30)).toBe('30');
    expect(tickFormatter(0.025, 'en-US').format(0.075)).toBe('0.075');
    expect(tickFormatter(niceScale(0.09).step, 'en-US').format(0.075)).toBe('0.075');
  });
});
