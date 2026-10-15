<script lang="ts">
  import { onMount } from 'svelte';
  import MetricCards from './components/MetricCards.svelte';
  import RangeSelector from './components/RangeSelector.svelte';
  import StatusChart from './components/StatusChart.svelte';
  import HistoryTable from './components/HistoryTable.svelte';
  import { connect, type ConnectionStatus } from './lib/connection';
  import type { Snapshot } from './lib/protocol';
  import { chartScales, historicalSeries, liveSeries, type ChartScales, type Range } from './lib/stats';
  let snapshot = $state.raw<Snapshot | null>(null);
  let status = $state<ConnectionStatus>('connecting');
  const live = $derived(status === 'live');
  const statusLabel = $derived(status === 'live' ? 'Live' : status === 'offline' ? 'Offline' : status === 'connecting' ? 'Connecting…' : 'Reconnecting…');
  let range = $state<Range>(0), table = $state(false);
  let axes = $state.raw<ChartScales>(chartScales(null, [], '1970-01-01T00:00:00Z', 0, ''));
  let resetAxes = false;
  let theme = $state<'auto' | 'light' | 'dark'>('auto');
  const points = $derived(snapshot ? range === 0 ? liveSeries(snapshot) : historicalSeries(snapshot, range) : []);
  const subtitle = $derived(range === 0 ? 'Completed seconds · last 5 minutes' : `1-minute averages · last ${range === 60 ? 'hour' : range === 360 ? '6 hours' : '24 hours'}`);
  function updateAxes(next: Snapshot) {
    const series = range === 0 ? liveSeries(next) : historicalSeries(next, range);
    axes = chartScales(resetAxes ? null : axes, series, next.updated_at, range, next.epoch);
  }
  function selectRange(next: Range) {
    range = next;
    if (snapshot) updateAxes(snapshot);
  }
  onMount(() => connect(next => { updateAxes(next); resetAxes = false; snapshot = next; }, value => { status = value; }, () => { resetAxes = true; }));
  function changeTheme() {
    theme = theme === 'auto' ? 'light' : theme === 'light' ? 'dark' : 'auto';
    if (theme === 'auto') document.documentElement.removeAttribute('data-theme');
    else document.documentElement.setAttribute('data-theme', theme);
  }
</script>

<main>
  <header>
    <div class="brand">ai-pool <span>· live status</span></div>
    <div class="spacer"></div>
    <div class="status" role="status">
      <svg viewBox="0 0 16 16" aria-hidden="true" class={live ? 'good' : status === 'offline' ? 'critical' : ''}>
        {#if live}<circle cx="8" cy="8" r="5" fill="currentColor" />
        {:else if status === 'offline'}<circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="1.5" /><path d="M5 5L11 11M11 5L5 11" stroke="currentColor" stroke-width="1.5" />
        {:else}<circle cx="8" cy="8" r="5" fill="none" stroke="currentColor" stroke-width="1.5" />{/if}
      </svg>
      {statusLabel}
    </div>
    <button class="theme" type="button" onclick={changeTheme}>Theme: {theme}</button>
  </header>
  <div class:stale={!live && snapshot !== null}>
    <MetricCards {snapshot} />
    <RangeSelector {range} select={selectRange} />
    <figure class="card chart">
      <div class="chart-head">
        <h2>Questions answered and average prompt size</h2>
        <button class="linkish" type="button" aria-expanded={table} aria-controls="history-table" onclick={() => { table = !table; }}>{table ? 'Hide table' : 'Show table'}</button>
      </div>
      <div class="subtitle">{subtitle} · Axes start at zero and scale independently.</div>
      {#key range}<StatusChart {points} {axes} isLive={range === 0} />{/key}
      {#if table}<HistoryTable {points} isLive={range === 0} />{/if}
    </figure>
  </div>
  <footer>Totals across all miners and models. Live updates every second; history covers the last 24 hours and restarts with the pool. Offline data refreshes every 10 seconds. For h2o, prompt size is the longest single-question prompt per request.</footer>
</main>
