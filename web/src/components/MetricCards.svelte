<script lang="ts">
  import type { Snapshot } from '../lib/protocol';
  import { format, formatInt, promptSeries, answeredHourNote, answeredHourSeries, sparkPath, sparkDots } from '../lib/stats';
  let { snapshot }: { snapshot: Snapshot | null } = $props();
  const metrics = $derived([
    { key: 'miners', label: 'Miners connected', value: snapshot?.totals.miners, unit: '', note: 'last hour', values: snapshot?.history.miners.slice(-60) ?? [], help: '' },
    { key: 'answered', label: 'Questions answered in the last hour', value: snapshot?.totals.answered_last_hour, unit: '', note: snapshot ? answeredHourNote(snapshot) : 'last 60 minutes', values: snapshot ? answeredHourSeries(snapshot) : [], help: 'Current minute so far plus the 59 most recent completed minutes, including the partial startup minute while it is in the window. Sparkline: answered questions per completed minute.' },
    { key: 'queued', label: 'Questions waiting for a miner', value: snapshot?.totals.queued_questions, unit: '', note: 'last hour · peak per minute', values: snapshot?.history.queued.slice(-60) ?? [], help: 'Current waiting count; the sparkline retains each completed minute’s peak.' },
    { key: 'prompt', label: 'Average prompt size', value: snapshot?.totals.average_prompt_tokens, unit: ' tokens', note: 'per request, last 60 s', values: snapshot ? promptSeries(snapshot).slice(-60) : [], help: 'Pre-submit input count per answered request. For h2o, the longest single-question prompt, including shared state and the question template; not the sum across questions. Sparkline: completed minute averages over the last hour; gaps mean no answered requests.' },
  ]);
</script>
<section class="card hero" aria-labelledby="hero-label">
  <div class="label" id="hero-label">Questions answered per second</div>
  <div class="value">{snapshot ? format.format(snapshot.totals.answered_per_second) : '–'}</div>
  <div class="sub">60-second average</div>
</section>
<section class="kpis" aria-label="Pool totals">
  {#each metrics as metric (metric.key)}
    <div class="card tile" title={metric.help || undefined}>
      <div class="tile-content">
      <div><div class="label">{metric.label}</div><div class="value">{metric.value === undefined || metric.value === null ? '–' : formatInt.format(metric.value) + metric.unit}</div><div class="note">{metric.note}</div></div>
      <svg viewBox="0 0 140 40" width="140" height="40" aria-hidden="true"><path d={sparkPath(metric.values)} fill="none" stroke="var(--deemph)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />
        {#each sparkDots(metric.values) as dot (dot.i)}<circle cx={dot.x} cy={dot.y} r="2" fill="var(--deemph)" />{/each}
      </svg>
      </div>
    </div>
  {/each}
</section>
