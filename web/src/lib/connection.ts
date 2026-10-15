import { parseRest, parseSnapshot, parseTick, type Snapshot } from './protocol';
import { applyTick, replaceSnapshot } from './stats';

export type ConnectionStatus = 'connecting' | 'live' | 'reconnecting' | 'offline';

// All resources belong to this instance and are released by stop().
export function connect(onSnapshot: (snapshot: Snapshot) => void, onStatus: (status: ConnectionStatus) => void, onResync?: () => void): () => void {
  let state: Snapshot | null = null;
  let source: EventSource | null = null;
  let controller: AbortController | null = null;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let snapshotTimer: ReturnType<typeof setTimeout> | undefined;
  let watchdogTimer: ReturnType<typeof setTimeout> | undefined;
  let lastPublicationAt: number | undefined;
  let status: ConnectionStatus = 'connecting';
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let stopped = false, backoff = 1000, generation = 0;
  let healthySince: number | undefined;
  const visible = () => !stopped && !document.hidden;
  function clearPolling() {
    clearTimeout(pollTimer); pollTimer = undefined;
    controller?.abort(); controller = null;
  }
  function setStatus(next: ConnectionStatus) { status = next; onStatus(next); }
  function closeStream() {
    clearTimeout(snapshotTimer); snapshotTimer = undefined;
    clearTimeout(watchdogTimer); watchdogTimer = undefined;
    source?.close(); source = null; healthySince = undefined;
  }
  function watchPublications() {
    clearTimeout(watchdogTimer);
    if (lastPublicationAt === undefined) { failed('reconnecting'); return; }
    const remaining = 5000 - (Date.now() - lastPublicationAt);
    if (remaining <= 0) { failed('reconnecting'); return; }
    watchdogTimer = setTimeout(() => failed('reconnecting'), remaining);
  }
  function advanced() {
    healthySince ??= Date.now();
    lastPublicationAt = Date.now();
    watchPublications(); setStatus('live');
  }
  function update(next: Snapshot) {
    const result = replaceSnapshot(state, next);
    state = result.state;
    if (result.changed) {
      lastPublicationAt = Date.now();
      onSnapshot(next);
    }
    return result.changed;
  }
  async function poll(token: number) {
    if (!visible() || source || token !== generation) return;
    controller?.abort();
    pollTimer = setTimeout(() => void poll(token), 10000);
    const request = new AbortController(); controller = request;
    try {
      const response = await fetch('/v1/stats', { cache: 'no-store', signal: request.signal });
      if (!response.ok) throw new Error('stats unavailable');
      const next = parseRest(await response.json());
      if (!next) throw new Error('invalid stats');
      if (!request.signal.aborted && visible() && token === generation && !source) update(next);
    } catch { /* Keep the last valid data and the current fallback indicator. */ }
    finally {
      if (controller === request) controller = null;
    }
  }
  function failed(next: ConnectionStatus = 'offline') {
    if (!visible()) return;
    closeStream(); clearPolling(); setStatus(next);
    const token = ++generation;
    void poll(token);
    clearTimeout(retryTimer);
    const delay = Math.min(60000, backoff * (0.8 + Math.random() * 0.4));
    backoff = Math.min(60000, backoff * 2);
    retryTimer = setTimeout(open, delay);
  }
  function open() {
    if (!visible()) return;
    clearTimeout(retryTimer); retryTimer = undefined;
    clearPolling(); closeStream(); ++generation;
    if (state) setStatus('reconnecting');
    let current: EventSource;
    try { current = new EventSource('/v1/stats/stream'); }
    catch { failed(); return; }
    source = current;
    let receivedSnapshot = false;
    const active = () => visible() && source === current;
    snapshotTimer = setTimeout(() => { if (active()) failed('reconnecting'); }, 10000);
    current.addEventListener('snapshot', event => {
      if (!active()) return;
      try {
        const next = parseSnapshot(JSON.parse((event as MessageEvent).data));
        if (!next) { failed(); return; }
        const changed = update(next); receivedSnapshot = true;
        clearTimeout(snapshotTimer); snapshotTimer = undefined;
        if (changed) {
          advanced();
        } else {
          // A repeated snapshot cannot make frozen data fresh again.
          watchPublications();
        }
      } catch { failed(); }
    });
    current.addEventListener('tick', event => {
      if (!active()) return;
      try {
        const tick = parseTick(JSON.parse((event as MessageEvent).data));
        if (!tick || !receivedSnapshot) { failed(); return; }
        const result = applyTick(state, tick);
        if (result.resync) { onResync?.(); failed(); return; }
        state = result.state;
        if (result.changed && state) {
          onSnapshot(state); advanced();
          if (Date.now() - healthySince! >= 30000) backoff = 1000;
        }
      } catch { failed(); }
    });
    current.onerror = () => { if (active()) failed(); };
  }
  function visibility() {
    ++generation; clearTimeout(retryTimer); retryTimer = undefined;
    clearPolling(); closeStream();
    setStatus(state ? 'reconnecting' : status);
    if (visible()) open();
  }
  setStatus('connecting');
  document.addEventListener('visibilitychange', visibility);
  open();
  return () => {
    stopped = true; ++generation; clearTimeout(retryTimer);
    clearPolling(); closeStream(); document.removeEventListener('visibilitychange', visibility);
  };
}
