import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { connect } from './connection';
import { snapshot, t } from './fixtures';
import type { Tick } from './protocol';
class DocumentMock extends EventTarget { hidden = false }
class SourceMock extends EventTarget {
  static instances: SourceMock[] = [];
  closed = false;
  onerror: (() => void) | null = null;
  constructor(readonly url: string) { super(); SourceMock.instances.push(this); }
  close() { this.closed = true; }
  emit(type: string, value: unknown) { this.dispatchEvent(Object.assign(new Event(type), { data: JSON.stringify(value) })); }
}
let doc: DocumentMock;
let fetchMock: ReturnType<typeof vi.fn>;
let cleanup: (() => void) | undefined;
const current = () => SourceMock.instances.at(-1)!;
beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(0); vi.spyOn(Math, 'random').mockReturnValue(0.5);
  doc = new DocumentMock(); SourceMock.instances = [];
  fetchMock = vi.fn(async () => ({ ok: true, json: async () => ({ ...snapshot(), ...snapshot().totals }) }));
  vi.stubGlobal('document', doc); vi.stubGlobal('EventSource', SourceMock); vi.stubGlobal('fetch', fetchMock);
});
afterEach(() => { cleanup?.(); cleanup = undefined; vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
describe('connection lifetime and fallback', () => {
  it('starts neutral and times out initial silence after ten seconds', async () => {
    const status = vi.fn(); cleanup = connect(vi.fn(), status);
    const source = current();
    expect(status).toHaveBeenLastCalledWith('connecting');
    await vi.advanceTimersByTimeAsync(9999);
    expect(source.closed).toBe(false); expect(fetchMock).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(source.closed).toBe(true);
    expect(status).toHaveBeenLastCalledWith('reconnecting');
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(SourceMock.instances).toHaveLength(2);
    expect(status).toHaveBeenLastCalledWith('reconnecting');
  });
  it('falls back five seconds after the last advancing tick and recovers to Live', async () => {
    const status = vi.fn(); cleanup = connect(vi.fn(), status);
    const source = current(), s = snapshot(); source.emit('snapshot', s);
    await vi.advanceTimersByTimeAsync(4000);
    const tick: Tick = { version: 1, epoch: s.epoch, seq: s.seq + 1, updated_at: t(182), totals: s.totals,
      second: { t: t(181), answered: 0, queued: 0, miners: 0 }, minute: null };
    source.emit('tick', tick);
    await vi.advanceTimersByTimeAsync(4999);
    expect(source.closed).toBe(false); expect(status).toHaveBeenLastCalledWith('live');
    await vi.advanceTimersByTimeAsync(1);
    expect(source.closed).toBe(true); expect(status).toHaveBeenLastCalledWith('reconnecting');
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1000);
    current().emit('snapshot', snapshot(183, 3));
    expect(status).toHaveBeenLastCalledWith('live');
    await vi.advanceTimersByTimeAsync(4999);
    expect(current().closed).toBe(false);
  });
  it('does not refresh the watchdog for duplicates or keep-alive activity', async () => {
    const status = vi.fn(); cleanup = connect(vi.fn(), status);
    const source = current(), s = snapshot(); source.emit('snapshot', s);
    await vi.advanceTimersByTimeAsync(4000);
    source.emit('snapshot', s);
    source.emit('tick', { ...s, second: s.live[0], minute: null });
    source.dispatchEvent(new Event('message'));
    await vi.advanceTimersByTimeAsync(1000);
    expect(source.closed).toBe(true); expect(status).toHaveBeenLastCalledWith('reconnecting');
    await vi.advanceTimersByTimeAsync(1000);
    const replacement = current(); replacement.emit('snapshot', s);
    expect(replacement.closed).toBe(true); expect(status).toHaveBeenLastCalledWith('reconnecting');
  });
  it('clears deadlines and watchdogs on hide, connection replacement and cleanup', async () => {
    const status = vi.fn(); cleanup = connect(vi.fn(), status);
    expect(vi.getTimerCount()).toBe(1);
    doc.hidden = true; doc.dispatchEvent(new Event('visibilitychange'));
    expect(vi.getTimerCount()).toBe(0);
    expect(status).toHaveBeenLastCalledWith('connecting');
    doc.hidden = false; doc.dispatchEvent(new Event('visibilitychange'));
    current().emit('snapshot', snapshot());
    expect(vi.getTimerCount()).toBe(1);
    await vi.advanceTimersByTimeAsync(1000);
    doc.hidden = true; doc.dispatchEvent(new Event('visibilitychange'));
    expect(vi.getTimerCount()).toBe(0);
    doc.hidden = false; doc.dispatchEvent(new Event('visibilitychange'));
    current().emit('snapshot', snapshot(182, 2));
    await vi.advanceTimersByTimeAsync(4000);
    expect(current().closed).toBe(false);
    expect(status).toHaveBeenLastCalledWith('live');
    cleanup(); cleanup = undefined;
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(20000);
    expect(fetchMock).not.toHaveBeenCalled();
  });
  it('owns one stream, validates events and closes on a sequence gap', async () => {
    const update = vi.fn(), status = vi.fn(), resync = vi.fn(); cleanup = connect(update, status, resync);
    const source = current(), s = snapshot();
    source.emit('snapshot', s);
    expect(update).toHaveBeenCalledWith(s); expect(status).toHaveBeenLastCalledWith('live');
    expect(resync).not.toHaveBeenCalled();
    source.emit('tick', { ...s, seq: s.seq + 2, updated_at: t(182), second: { ...s.live[0], t: s.updated_at }, minute: null });
    expect(source.closed).toBe(true); expect(status).toHaveBeenLastCalledWith('offline');
    expect(resync).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(SourceMock.instances).toHaveLength(2);
    expect(SourceMock.instances.filter(s => !s.closed)).toHaveLength(1);
  });
  it('polls immediately and every ten seconds during capped backoff', async () => {
    cleanup = connect(vi.fn(), vi.fn());
    for (const delay of [1000, 2000, 4000, 8000, 16000, 32000, 60000]) {
      const start = SourceMock.instances.length;
      current().onerror!();
      await vi.advanceTimersByTimeAsync(delay - 1);
      expect(SourceMock.instances).toHaveLength(start);
      await vi.advanceTimersByTimeAsync(1);
      expect(SourceMock.instances).toHaveLength(start + 1);
    }
    expect(fetchMock.mock.calls.length).toBeGreaterThan(15);
    expect(fetchMock.mock.calls[0][0]).toBe('/v1/stats');
  });
  it('marks fallback Offline even when polling succeeds and aborts in-flight requests', async () => {
    let signal: AbortSignal | undefined;
    fetchMock.mockImplementation((_url: string, options: RequestInit) => {
      signal = options.signal as AbortSignal;
      return new Promise(() => {});
    });
    const status = vi.fn(); cleanup = connect(vi.fn(), status);
    current().onerror!();
    expect(status).toHaveBeenLastCalledWith('offline');
    expect(signal?.aborted).toBe(false);
    doc.hidden = true; doc.dispatchEvent(new Event('visibilitychange'));
    expect(signal?.aborted).toBe(true);
    await vi.advanceTimersByTimeAsync(120000);
    expect(SourceMock.instances).toHaveLength(1);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
  it('aborts a stalled poll and starts the next one at ten seconds', async () => {
    cleanup = connect(vi.fn(), vi.fn());
    for (const delay of [1000, 2000, 4000, 8000]) { current().onerror!(); await vi.advanceTimersByTimeAsync(delay); }
    const signals: AbortSignal[] = [];
    fetchMock.mockImplementation((_url: string, options: RequestInit) => {
      signals.push(options.signal as AbortSignal);
      return new Promise(() => {});
    });
    current().onerror!();
    expect(signals).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(10000);
    expect(signals).toHaveLength(2);
    expect(signals[0].aborted).toBe(true);
    expect(signals[1].aborted).toBe(false);
    cleanup(); cleanup = undefined;
    expect(signals[1].aborted).toBe(true);
  });
  it('stops while hidden, reconnects when visible, and cleans up listeners and timers', async () => {
    doc.hidden = true; cleanup = connect(vi.fn(), vi.fn());
    expect(SourceMock.instances).toHaveLength(0);
    doc.hidden = false; doc.dispatchEvent(new Event('visibilitychange'));
    const source = current(); source.onerror!();
    await vi.advanceTimersByTimeAsync(0);
    cleanup(); cleanup = undefined;
    expect(source.closed).toBe(true); expect(vi.getTimerCount()).toBe(0);
    doc.dispatchEvent(new Event('visibilitychange'));
    await vi.advanceTimersByTimeAsync(120000);
    expect(SourceMock.instances).toHaveLength(1);
  });
  it('rejects malformed snapshots and restart ticks, accepting a replacement restart snapshot', () => {
    const update = vi.fn(); cleanup = connect(update, vi.fn());
    current().emit('snapshot', { bad: true });
    expect(current().closed).toBe(true); expect(update).not.toHaveBeenCalled();
    cleanup(); cleanup = connect(update, vi.fn());
    const s = snapshot(); current().emit('snapshot', s);
    const restart = { ...s, epoch: '00000000-0000-4000-8000-000000000002', seq: 0 };
    current().emit('snapshot', restart);
    expect(update).toHaveBeenLastCalledWith(restart);
    current().emit('tick', { ...s, second: { ...s.live[0], t: s.updated_at }, minute: null });
    expect(current().closed).toBe(true);
  });
  it('resets retry delay after thirty seconds of healthy ticks', async () => {
    cleanup = connect(vi.fn(), vi.fn());
    for (const delay of [1000, 2000, 4000]) { current().onerror!(); await vi.advanceTimersByTimeAsync(delay); }
    let s = snapshot(181, 2); current().emit('snapshot', s);
    for (let i = 0; i < 30; i++) {
      await vi.advanceTimersByTimeAsync(1000);
      const tick: Tick = { version: 1, epoch: s.epoch, seq: s.seq + 1,
        updated_at: new Date(Date.parse(s.updated_at) + 1000).toISOString().replace('.000Z', 'Z'),
        totals: s.totals, second: { t: s.updated_at, answered: 0, queued: 0, miners: 0 }, minute: null };
      current().emit('tick', tick);
      s = { ...s, updated_at: tick.updated_at, seq: tick.seq, live: [...s.live, tick.second] };
    }
    const count = SourceMock.instances.length; current().onerror!();
    await vi.advanceTimersByTimeAsync(1000);
    expect(SourceMock.instances).toHaveLength(count + 1);
  });
});
