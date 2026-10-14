<script lang="ts">
  import type { Snapshot } from '../lib/protocol';
  import { format, formatInt } from '../lib/stats';
  let { snapshot }: { snapshot: Snapshot | null } = $props();
  const metrics = $derived([
    { key: 'miners', label: 'Miners connected', value: snapshot?.totals.miners, values: snapshot?.history.miners.slice(-60) ?? [] },
    { key: 'queued', label: 'Questions queued', value: snapshot?.totals.queued_questions, values: snapshot?.history.queued.slice(-60) ?? [] },
  ]);
  function spark(values: number[]): string {
    const max = Math.max(1, ...values);
    return values.map((v, i) => `${i ? 'L' : 'M'}${values.length === 1 ? 140 : i / (values.length - 1) * 140},${38 - v / max * 34}`).join('');
  }
</script>
<section class="card hero" aria-labelledby="hero-label">
  <div class="label" id="hero-label">Questions answered per second</div>
  <div class="value">{snapshot ? format.format(snapshot.totals.answered_per_second) : '–'}</div>
  <div class="sub">60-second average</div>
</section>
<section class="kpis" aria-label="Pool totals">
  {#each metrics as metric (metric.key)}
    <div class="card tile">
      <div><div class="label">{metric.label}</div><div class="value">{metric.value === undefined ? '–' : formatInt.format(metric.value)}</div><div class="note">last hour</div></div>
      <svg viewBox="0 0 140 40" aria-hidden="true"><path d={spark(metric.values)} fill="none" stroke="var(--deemph)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </div>
  {/each}
</section>
