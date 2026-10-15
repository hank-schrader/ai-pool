import { describe, expect, it } from 'vitest';
import { parseRest, parseSnapshot, parseTick, type Snapshot, type Tick } from './protocol';
import { applyTick, historicalSeries, liveSeries, replaceSnapshot, niceScale, tickFormatter, promptAverage, promptSeries, sparkPath, sparkDots, alignedScale, tokenTickFormatter, answeredHourNote, answeredHourSeries, chartScales, type ChartScales, type Range } from './stats';
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
    expect(historicalSeries(rolled, 60).at(-1)).toMatchObject({ t: 180000, v: 1, queued: 0, miners: 1 });
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

describe('additive v1 workload metrics', () => {
  function workload() {
    const s = snapshot();
    s.totals = { ...s.totals, answering_questions: 4, average_prompt_tokens: 1500 };
    s.live = [{ ...s.live[0], answering: 4, requests: 2, prompt_tokens: 3000 }];
    s.history = { ...s.history, answering: [4], requests: [2], prompt_tokens: [3000] };
    return s;
  }
  it('parses extended and legacy v1 snapshots and REST responses', () => {
    const s = workload();
    expect(parseSnapshot(s)).toEqual(s);
    expect(parseRest({ ...s, ...s.totals })).toEqual(s);
    expect(parseSnapshot(snapshot())).not.toBeNull();
    expect(parseSnapshot({ ...s, totals: { ...s.totals, average_prompt_tokens: null } })).not.toBeNull();
    const next = { ...tick(s), second: { ...s.live[0], t: s.updated_at }, minute: null };
    expect(parseTick(next)).toEqual(next);
  });
  it('rejects invalid added counts, averages and parallel arrays', () => {
    const s = workload();
    for (const bad of [
      { ...s, totals: { ...s.totals, answering_questions: -1 } },
      { ...s, totals: { ...s.totals, average_prompt_tokens: Infinity } },
      { ...s, totals: { ...s.totals, average_prompt_tokens: -1 } },
      { ...s, live: [{ ...s.live[0], requests: 0 }] },
      { ...s, live: [{ ...s.live[0], answering: 0.5 }] },
      { ...s, live: [{ ...s.live[0], prompt_tokens: undefined }] },
      { ...s, history: { ...s.history, answering: [] } },
      { ...s, history: { ...s.history, requests: [0] } },
      { ...s, history: { ...s.history, prompt_tokens: [-1] } },
      { ...s, history: { ...s.history, requests: undefined } },
    ]) expect(parseSnapshot(bad)).toBeNull();
  });
  it('computes per-request averages, with empty buckets as gaps instead of zero', () => {
    expect(promptAverage(2, 3000)).toBe(1500);
    expect(promptAverage(4, 3000)).toBe(750);
    expect(promptAverage(1, 0)).toBe(0);
    expect(promptAverage(0, 0)).toBeNull();
    expect(promptAverage(undefined, undefined)).toBeNull();
    const s = workload();
    s.history = { ...s.history, answered: [7, 0, 3], queued: [1, 0, 0], miners: [2, 2, 2],
      answering: [4, 0, 0], requests: [2, 0, 1], prompt_tokens: [3000, 0, 9000] };
    expect(promptSeries(s)).toEqual([1500, null, 9000]);
    expect(sparkPath([2, null, 4])).toBe('M0,21M140,4');
    expect(sparkPath([2, 4, null, 2, 4])).toBe('M0,21L35,4M105,21L140,4');
    expect(sparkPath([null, undefined])).toBe('');
    expect(sparkDots([2, null, 4])).toEqual([{ i: 0, x: 0, y: 21 }, { i: 2, x: 140, y: 4 }]);
    expect(sparkDots([2, 4, null])).toEqual([]);
    expect(sparkDots([null, 0, null])).toEqual([{ i: 1, x: 70, y: 38 }]);
    expect(sparkPath([0, 0])).toBe('M0,38L140,38');
  });
  it('keeps added metrics through ticks and authoritative minute replacement', () => {
    const s = workload(); s.updated_at = t(239); s.live[0].t = t(238);
    const next = { ...tick(s), second: { ...s.live[0], t: t(239), requests: 1, prompt_tokens: 9000 },
      minute: { t: t(180), answered: 3, queued: 7, answering: 2, miners: 2, requests: 3, prompt_tokens: 12000 } };
    const rolled = applyTick(s, next).state!;
    expect(rolled.history.answering).toEqual([4, 2]);
    expect(rolled.history.requests).toEqual([2, 3]);
    expect(rolled.history.prompt_tokens).toEqual([3000, 12000]);
    expect(historicalSeries(rolled, 60).at(-1)).toMatchObject({ queued: 7, answering: 2, requests: 3, prompt_tokens: 12000 });
    expect(promptSeries(rolled)).toEqual([1500, 4000]);
    expect(liveSeries(rolled).at(-1)).toMatchObject({ requests: 1, prompt_tokens: 9000, answering: 4 });
    const soFar = historicalSeries(s, 60).at(-1)!;
    expect(soFar).toMatchObject({ provisional: true, queued: 3, requests: 2, prompt_tokens: 3000 });
    expect(promptAverage(soFar.requests, soFar.prompt_tokens)).toBe(1500);
  });
});

describe('hourly answered tile', () => {
  it('validates optional hourly totals in REST, snapshots and ticks', () => {
    const s = snapshot(); s.totals.answered_last_hour = 1234;
    expect(parseSnapshot(s)).toEqual(s);
    expect(parseTick(tick(s))?.totals.answered_last_hour).toBe(1234);
    expect(parseRest({ ...s, ...s.totals })).toEqual(s);
    for (const value of [-1, 1.5, Infinity, null]) {
      expect(parseSnapshot({ ...s, totals: { ...s.totals, answered_last_hour: value } })).toBeNull();
    }
  });
  it('notes startup coverage through the first hour and builds per-minute counts', () => {
    const s = snapshot();
    s.history.answered = [];
    expect(answeredHourNote(s)).toBe('since pool restart (0 min)');
    s.updated_at = t(240);
    s.live = [{ ...s.live[0], t: t(239) }];
    expect(answeredHourNote(s)).toBe('since pool restart (1 min)');
    s.history.answered = [10];
    expect(answeredHourNote(s)).toBe('since pool restart (2 min)');
    s.history.answered = Array.from({ length: 58 }, () => 10);
    expect(answeredHourNote(s)).toBe('since pool restart (59 min)');
    s.history.answered.push(10);
    expect(answeredHourNote(s)).toBe('last 60 minutes');
    s.live.push({ ...s.live[0], t: t(240), answered: 5 });
    s.updated_at = t(241);
    expect(answeredHourSeries(s)).toEqual(Array(59).fill(10));
    s.history.answered.push(100, 20);
    expect(answeredHourSeries(s)).toEqual([...Array(58).fill(10), 100, 20]);
  });
});

describe('dual-axis prompt series', () => {
  it('weights the last 10 completed seconds by requests and leaves empty windows as gaps', () => {
    const s = snapshot(195);
    s.live = Array.from({ length: 15 }, (_, i) => ({ t: t(180 + i), answered: 0, queued: 0, miners: 1, requests: 0, prompt_tokens: 0 }));
    s.live[1] = { ...s.live[1], requests: 1, prompt_tokens: 1000 };
    s.live[2] = { ...s.live[2], requests: 3, prompt_tokens: 9000 };
    const points = liveSeries(s);
    expect(points.slice(0, 9).map(p => p.prompt)).toEqual(Array(9).fill(null));
    expect(points.slice(0, 9).every(p => p.promptPending)).toBe(true);
    expect(points[9]).toMatchObject({ prompt: 2500, promptPending: false });
    expect(points[10].prompt).toBe(2500);
    expect(points[11].prompt).toBe(3000);
    expect(points[12].prompt).toBeNull();
    s.live[14] = { ...s.live[14], requests: 1, prompt_tokens: 0 };
    expect(liveSeries(s)[14].prompt).toBe(0);
    s.live[13].requests = undefined;
    expect(liveSeries(s)[14].prompt).toBeNull();
  });
  it('uses minute averages with gaps, including the provisional current minute', () => {
    const s = snapshot();
    s.history = { ...s.history, answered: [10, 0, 20], miners: [1, 1, 1], queued: [0, 0, 0], requests: [1, 0, 4], prompt_tokens: [1000, 0, 10000] };
    s.live = [{ ...s.live[0], requests: 2, prompt_tokens: 3000 }];
    expect(historicalSeries(s, 60).map(p => p.prompt)).toEqual([1000, null, 2500, null]);
  });
  it('gates provisional prompt values until ten observed completed seconds in every history range', () => {
    for (const range of [60, 360, 1440] as const) {
      for (let seconds = 1; seconds <= 10; seconds++) {
        const s = snapshot(180 + seconds);
        s.live = Array.from({ length: seconds }, (_, i) => ({ t: t(180 + i), answered: 1, queued: 0, miners: 1, requests: 1, prompt_tokens: 20000 }));
        const points = historicalSeries(s, range);
        expect(points.at(-1)).toMatchObject({ prompt: seconds < 10 ? null : 20000, promptPending: seconds < 10 });
        const axes = chartScales(null, points, s.updated_at, range, s.epoch);
        if (seconds < 10) {
          expect(axes.right.scale).toEqual(chartScales(null, points.slice(0, -1), s.updated_at, range, s.epoch).right.scale);
        } else expect(axes.right.scale.top).toBeGreaterThanOrEqual(20000 * 1.05);
      }
    }
    const s = snapshot(200);
    s.live = Array.from({ length: 9 }, (_, i) => ({ t: t(191 + i), answered: 1, queued: 0, miners: 1, requests: 1, prompt_tokens: 1000 }));
    expect(historicalSeries(s, 60).at(-1)).toMatchObject({ incomplete: true, prompt: null, promptPending: true });
  });
  it('excludes a sparse long-prompt edge from the axis while preserving complete weighted windows', () => {
    const s = snapshot(200);
    s.live = Array.from({ length: 20 }, (_, i) => ({ t: t(180 + i), answered: 1, queued: 0, miners: 1, requests: 10, prompt_tokens: 10000 }));
    s.live[0] = { ...s.live[0], requests: 1, prompt_tokens: 20000 };
    const points = liveSeries(s);
    expect(points[0].prompt).toBeNull();
    expect(points[9].prompt).toBeCloseTo(110000 / 91);
    expect(points[10].prompt).toBe(1000);
    expect(chartScales(null, points, s.updated_at, 0, s.epoch).right.scale.top).toBe(1500);
  });
  it('matches left-axis interval counts with nice zero-based token steps', () => {
    for (const leftMax of [0, 0.09, 1, 6, 13, 120, 999]) {
      const left = niceScale(leftMax), intervals = Math.round(left.top / left.step);
      for (const max of [0, 0.009, 450, 1400, 4000, 1000000]) {
        const right = alignedScale(max, intervals);
        expect(right.top).toBeGreaterThanOrEqual(max);
        expect(right.top / right.step).toBeCloseTo(intervals);
        for (let i = 0; i <= intervals; i++) expect(i * right.step / right.top).toBeCloseTo(i * left.step / left.top);
      }
    }
  });
  it('formats token ticks using step-aware precision and thousands', () => {
    expect([0, 1000, 2000, 3000, 4000].map(tokenTickFormatter(1000, 'en-US'))).toEqual(['0', '1k', '2k', '3k', '4k']);
    expect([0, 500, 1000, 1500].map(tokenTickFormatter(500, 'en-US'))).toEqual(['0', '500', '1,000', '1,500']);
    expect(tokenTickFormatter(2500, 'en-US')(7500)).toBe('7.5k');
    expect(tokenTickFormatter(0.25, 'en-US')(0.5)).toBe('0.50');
  });
});

describe('axis hysteresis using publication timestamps', () => {
  const epoch = snapshot().epoch;
  const update = (previous: ChartScales | null, second: number, max = 19, prompt = 1784, range: Range = 0, nextEpoch = epoch) =>
    chartScales(previous, [{ t: second * 1000, v: max, prompt }], t(second), range, nextEpoch);
  it('holds both tops for 30 seconds of fitting data, then shrinks', () => {
    let axes = update(null, 100, 90, 9000);
    const large = axes;
    axes = update(axes, 101);
    // Repeated renders/publications at one timestamp cannot advance the clock.
    for (let i = 0; i < 40; i++) axes = update(axes, 101);
    axes = update(axes, 130);
    expect(axes.left.scale).toEqual(large.left.scale);
    expect(axes.right.scale).toEqual(large.right.scale);
    axes = update(axes, 131);
    expect(axes.left.scale.top).toBe(20);
    expect(axes.right.scale.top).toBe(2000);
  });
  it('grows immediately on real question and prompt spikes', () => {
    let axes = update(null, 100);
    axes = update(axes, 101, 90, 20000);
    expect(axes.left.scale.top).toBeGreaterThanOrEqual(90 * 1.05);
    expect(axes.right.scale.top).toBeGreaterThanOrEqual(20000 * 1.05);
    axes = update(axes, 102);
    const held = axes;
    axes = update(axes, 103, 190, 40000);
    expect(axes.left.scale.top).toBeGreaterThan(held.left.scale.top);
    expect(axes.right.scale.top).toBeGreaterThan(held.right.scale.top);
  });
  it('keeps the right scale stable as the left max wiggles across a nice boundary', () => {
    let axes = update(null, 100, 21);
    const original = axes;
    expect(axes.left.scale).toEqual({ top: 30, step: 10 });
    for (const [second, max] of [[101, 19], [115, 21], [116, 19], [130, 21], [131, 19], [160, 19]]) {
      axes = update(axes, second, max);
      expect(axes.left.scale).toEqual(original.left.scale);
      expect(axes.right.scale).toEqual(original.right.scale);
    }
    axes = update(axes, 161);
    expect(axes.left.scale).toEqual({ top: 20, step: 5 });
    expect(axes.right.scale.top / axes.right.scale.step).toBe(4);
  });
  it('restarts the shrink clock when data stops fitting its smaller target', () => {
    let axes = update(null, 100, 90, 9000);
    axes = update(axes, 101);
    axes = update(axes, 120, 50, 5000);
    axes = update(axes, 131, 50, 5000);
    expect(axes.left.scale.top).toBe(100);
    expect(axes.right.scale.top).toBe(10000);
    axes = update(axes, 150, 50, 5000);
    expect(axes.left.scale.top).toBe(60);
    expect(axes.right.scale.top).toBeLessThan(10000);
  });
  it('requires another full delay before adopting an even smaller target', () => {
    let axes = update(null, 100, 90, 9000);
    axes = update(axes, 101, 50, 5000);
    axes = update(axes, 130);
    axes = update(axes, 131);
    expect(axes.left.scale.top).toBe(60);
    axes = update(axes, 132);
    axes = update(axes, 161);
    expect(axes.left.scale.top).toBe(60);
    axes = update(axes, 162);
    expect(axes.left.scale.top).toBe(20);
  });
  it('resets immediately on every range switch, epoch change and explicit resync', () => {
    const large = update(null, 100, 90, 9000);
    const fresh = update(null, 101);
    for (const range of [60, 360, 1440] as const) {
      const changed = update(large, 101, 19, 1784, range);
      expect(changed.left).toEqual(fresh.left);
      expect(changed.right).toEqual(fresh.right);
      const backToLive = update(update(changed, 102, 90, 9000, range), 103);
      expect(backToLive.left).toEqual(fresh.left);
      expect(backToLive.right).toEqual(fresh.right);
    }
    const restarted = update(large, 101, 19, 1784, 0, '00000000-0000-4000-8000-000000000002');
    expect(restarted.left).toEqual(fresh.left);
    expect(restarted.right).toEqual(fresh.right);
    const resynced = update(null, 101);
    expect(resynced.left).toEqual(fresh.left);
    expect(resynced.right).toEqual(fresh.right);
  });
  it('aligns every token tick with a zero-based left gridline through scale changes', () => {
    let axes: ChartScales | null = null;
    for (const [second, max, prompt] of [[100, 19, 1784], [101, 21, 1784], [102, 19, 1784], [132, 19, 1784], [133, 90, 20000], [134, 1, 10], [164, 1, 10]]) {
      axes = update(axes, second, max, prompt);
      const left = axes.left.scale, right = axes.right.scale;
      const intervals = Math.round(left.top / left.step);
      expect(right.top).toBeGreaterThanOrEqual(prompt * 1.05);
      for (let i = 0; i <= intervals; i++) expect(i * right.step / right.top).toBeCloseTo(i * left.step / left.top);
    }
  });
});
