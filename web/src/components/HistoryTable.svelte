<script lang="ts">
  import { format, formatInt, promptAverage, pointNote, type SeriesPoint } from '../lib/stats';
  let { points, isLive }: { points: SeriesPoint[]; isLive: boolean } = $props();
  const count = (value: number | undefined) => value === undefined ? '–' : formatInt.format(value);
</script>
<div class="tablewrap" id="history-table">
  <table>
    <caption class="sr-only">Questions answered per second and pool metrics, most recent first. Waiting counts are bucket peaks; miners and questions being answered are sampled at bucket boundaries.</caption>
    <thead><tr><th scope="col">Time</th><th scope="col" class="num">Questions / s</th>{#if isLive}<th scope="col" class="num">10-second average</th>{/if}<th scope="col" class="num">Miners</th><th scope="col" class="num">Being answered</th><th scope="col" class="num">Waiting peak</th><th scope="col" class="num">Answered requests</th><th scope="col" class="num">Prompt tokens</th><th scope="col" class="num">Average prompt tokens / request</th><th scope="col">Coverage</th></tr></thead>
    <tbody>
      {#each points.toReversed() as point (point.t)}
        {@const average = promptAverage(point.requests, point.prompt_tokens)}
        <tr><td>{new Date(point.t).toLocaleString()}</td><td class="num">{format.format(point.v)}</td>{#if isLive}<td class="num">{format.format(point.average ?? point.v)}</td>{/if}<td class="num">{count(point.miners)}</td><td class="num">{count(point.answering)}</td><td class="num">{count(point.queued)}</td><td class="num">{count(point.requests)}</td><td class="num">{count(point.prompt_tokens)}</td><td class="num">{average === null ? '–' : formatInt.format(average)}</td><td>{pointNote(point) || (isLive ? 'Completed second' : 'Completed minute')}</td></tr>
      {/each}
    </tbody>
  </table>
</div>
