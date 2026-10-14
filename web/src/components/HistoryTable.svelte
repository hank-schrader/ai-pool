<script lang="ts">
  import { format, pointNote, type SeriesPoint } from '../lib/stats';
  let { points, isLive }: { points: SeriesPoint[]; isLive: boolean } = $props();
</script>
<div class="tablewrap" id="history-table">
  <table>
    <caption class="sr-only">Questions answered per second, most recent first</caption>
    <thead><tr><th scope="col">Time</th><th scope="col" class="num">Questions / s</th>{#if isLive}<th scope="col" class="num">10-second average</th>{/if}<th scope="col">Coverage</th></tr></thead>
    <tbody>
      {#each points.toReversed() as point (point.t)}
        <tr><td>{new Date(point.t).toLocaleString()}</td><td class="num">{format.format(point.v)}</td>{#if isLive}<td class="num">{format.format(point.average ?? point.v)}</td>{/if}<td>{pointNote(point) || (isLive ? 'Completed second' : 'Completed minute')}</td></tr>
      {/each}
    </tbody>
  </table>
</div>
