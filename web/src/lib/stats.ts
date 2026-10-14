import type { Snapshot, Tick, Point } from './protocol';
export interface SeriesPoint { t: number; v: number; average?: number; provisional?: boolean; incomplete?: boolean; seconds?: number; queued?: number; miners?: number; answering?: number; requests?: number; prompt_tokens?: number }
export type Range = 0 | 60 | 360 | 1440;
export type Reduction = { state: Snapshot | null; resync: boolean; changed: boolean };
export function replaceSnapshot(state: Snapshot | null, next: Snapshot): Reduction {
  if (state && state.epoch === next.epoch && next.seq <= state.seq) return { state, resync: false, changed: false };
  return { state: next, resync: false, changed: true };
}
export function applyTick(state: Snapshot | null, tick: Tick): Reduction {
  if (!state || tick.epoch !== state.epoch) return { state, resync: true, changed: false };
  if (tick.seq <= state.seq) return { state, resync: false, changed: false };
  const second = Date.parse(tick.second.t);
  const adjacent = state.live.length ? second === Date.parse(state.live.at(-1)!.t) + 1000
    : second >= Date.parse(state.updated_at) && second <= Date.parse(state.updated_at) + 1000;
  if (tick.seq !== state.seq + 1 || !adjacent) return { state, resync: true, changed: false };
  let history = state.history;
  if (tick.minute) {
    if (history.answered.length && Date.parse(tick.minute.t) !== Date.parse(history.end) + 60000) return { state, resync: true, changed: false };
    history = { interval_seconds: 60, end: tick.minute.t,
      answered: [...history.answered, tick.minute.answered].slice(-1440),
      queued: [...history.queued, tick.minute.queued].slice(-1440),
      miners: [...history.miners, tick.minute.miners].slice(-1440),
      answering: appendMetric(history.answering, tick.minute.answering),
      requests: appendMetric(history.requests, tick.minute.requests),
      prompt_tokens: appendMetric(history.prompt_tokens, tick.minute.prompt_tokens) };
  }
  return { state: { version: 1, epoch: tick.epoch, seq: tick.seq, updated_at: tick.updated_at, totals: tick.totals,
    live: [...state.live, tick.second].slice(-300), history }, resync: false, changed: true };
}
function appendMetric(values: number[] | undefined, value: number | undefined): number[] | undefined {
  return values && value !== undefined ? [...values, value].slice(-1440) : undefined;
}
export function promptAverage(requests: number | undefined, tokens: number | undefined): number | null {
  return requests !== undefined && requests > 0 && tokens !== undefined ? tokens / requests : null;
}
export function promptSeries(state: Snapshot): (number | null)[] {
  return state.history.answered.map((_, i) => promptAverage(state.history.requests?.[i], state.history.prompt_tokens?.[i]));
}
export function sparkPath(values: (number | null | undefined)[]): string {
  const max = Math.max(1, ...values.map(v => v ?? 0));
  let connected = false;
  return values.map((v, i) => {
    if (v === null || v === undefined) { connected = false; return ''; }
    const command = connected ? 'L' : 'M'; connected = true;
    return `${command}${values.length === 1 ? 140 : i / (values.length - 1) * 140},${38 - v / max * 34}`;
  }).join('');
}
export function sparkDots(values: (number | null | undefined)[]): { i: number; x: number; y: number }[] {
  const max = Math.max(1, ...values.map(v => v ?? 0));
  return values.flatMap((v, i) => v !== null && v !== undefined
    && values[i - 1] == null && values[i + 1] == null
    ? [{ i, x: values.length === 1 ? 140 : i / (values.length - 1) * 140, y: 38 - v / max * 34 }] : []);
}
function metrics(point: Point): Pick<SeriesPoint, 'queued' | 'miners' | 'answering' | 'requests' | 'prompt_tokens'> {
  return { queued: point.queued, miners: point.miners, answering: point.answering, requests: point.requests, prompt_tokens: point.prompt_tokens };
}
export function liveSeries(state: Snapshot): SeriesPoint[] {
  let sum = 0;
  return state.live.map((p, i, points) => {
    sum += p.answered;
    if (i >= 10) sum -= points[i - 10].answered;
    return { ...metrics(p), t: Date.parse(p.t), v: p.answered, average: sum / Math.min(i + 1, 10) };
  });
}
export function historicalSeries(state: Snapshot, range: Exclude<Range, 0>): SeriesPoint[] {
  const h = state.history, end = Date.parse(h.end);
  const points: SeriesPoint[] = h.answered.map((v, i) => ({ t: end - (h.answered.length - i - 1) * 60000, v: v / 60,
    queued: h.queued[i], miners: h.miners[i], answering: h.answering?.[i], requests: h.requests?.[i], prompt_tokens: h.prompt_tokens?.[i] }));
  const minute = Math.floor(Date.parse(state.updated_at) / 60000) * 60000;
  const observed = state.live.filter(p => Date.parse(p.t) >= minute && Date.parse(p.t) < minute + 60000);
  if (observed.length) points.push({ t: minute, v: observed.reduce((sum, p) => sum + p.answered, 0) / observed.length,
    queued: Math.max(...observed.map(p => p.queued)), miners: observed.at(-1)!.miners, answering: observed.at(-1)!.answering,
    requests: observed.every(p => p.requests !== undefined) ? observed.reduce((sum, p) => sum + p.requests!, 0) : undefined,
    prompt_tokens: observed.every(p => p.prompt_tokens !== undefined) ? observed.reduce((sum, p) => sum + p.prompt_tokens!, 0) : undefined,
    provisional: true, incomplete: Date.parse(observed[0].t) !== minute, seconds: observed.length });
  return points.filter(p => p.t >= minute - range * 60000);
}
export const format = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
export const formatInt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
export const timeFormat = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
export function pointNote(p: SeriesPoint): string {
  return p.provisional ? `so far · ${p.seconds} completed seconds${p.incomplete ? ' · incomplete startup coverage' : ''}` : '';
}
export function niceScale(max: number): { step: number; top: number } {
  const raw = (max || 1) / 4, mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map(m => m * mag).find(s => s >= raw)!;
  return { step, top: Math.ceil((max || 1) / step) * step };
}

export function tickFormatter(step: number, locale?: string): Intl.NumberFormat {
  // Scientific notation avoids floating-point tails in clean 1/2/2.5/5 steps.
  const [coefficient, exponent] = step.toExponential(12).split('e');
  const fraction = (coefficient.split('.')[1] ?? '').replace(/0+$/, '').length;
  const decimals = Math.max(0, Math.min(20, fraction - Number(exponent)));
  return new Intl.NumberFormat(locale, { minimumFractionDigits: decimals, maximumFractionDigits: decimals });
}
