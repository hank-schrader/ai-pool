<script lang="ts">
  import { format, timeFormat, niceScale, tickFormatter, pointNote, type SeriesPoint } from '../lib/stats';
  let { points, isLive }: { points: SeriesPoint[]; isLive: boolean } = $props();
  let width = $state(960), hover = $state<number | null>(null);
  const W = $derived(Math.max(320, width));
  const scale = $derived(niceScale(Math.max(0, ...points.map(p => Math.max(p.v, p.average ?? 0))) * 1.05));
  const axisFormat = $derived(tickFormatter(scale.step));
  const ticks = $derived(Array.from({ length: Math.round(scale.top / scale.step) + 1 }, (_, i) => i * scale.step));
  const x = (i: number) => 36 + (W - 92) * (points.length === 1 ? 1 : i / (points.length - 1));
  const y = (v: number) => 10 + 264 * (1 - v / scale.top);
  const path = $derived(points.map((p, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)},${y(p.v).toFixed(1)}`).join(''));
  const average = $derived(points.map((p, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)},${y(p.average ?? p.v).toFixed(1)}`).join(''));
  const last = $derived(points.length - 1);
  const focused = $derived(hover === null ? undefined : points[hover]);
  const tooltipX = $derived(Math.max(8, Math.min(W - 258, x(hover ?? 0) - 125)));
  const tooltipY = $derived(focused ? Math.max(8, Math.min(185, y(focused.average ?? focused.v) - 100)) : 8);
  const xTicks = $derived.by(() => {
    const fit = Math.max(2, Math.min(6, Math.floor((W - 92) / 90)));
    const step = Math.max(1, Math.round(points.length / fit));
    return points.flatMap((p, i) => i % step === 0 ? [{ t: p.t, i }] : []);
  });
  function pointer(event: PointerEvent) {
    if (!points.length) return;
    const svg = event.currentTarget as SVGSVGElement, rect = svg.getBoundingClientRect();
    const position = (event.clientX - rect.left) * W / rect.width;
    hover = Math.max(0, Math.min(last, Math.round((position - 36) / (W - 92) * last)));
  }
  function key(event: KeyboardEvent) {
    if (!points.length) return;
    const step = event.shiftKey ? 10 : 1;
    if (event.key === 'Home') hover = 0;
    else if (event.key === 'End') hover = last;
    else if (event.key === 'ArrowLeft') hover = Math.max(0, (hover ?? last) - step);
    else if (event.key === 'ArrowRight') hover = Math.min(last, (hover ?? last) + step);
    else if (event.key === 'Escape') hover = null;
    else return;
    event.preventDefault();
  }
</script>
{#if isLive}
  <div class="legend" aria-label="Chart legend"><span><i class="legend-second"></i>Per second</span><span><i class="legend-average"></i>10-second average</span></div>
{/if}
<div class="plot" bind:clientWidth={width}>
  <svg viewBox={`0 0 ${W} 300`} tabindex="0" role="slider" aria-roledescription="chart time cursor" aria-valuemin="0" aria-valuemax={Math.max(0, last)} aria-valuenow={hover ?? Math.max(0, last)} aria-valuetext={focused ? `${new Date(focused.t).toLocaleString()}: ${format.format(focused.v)} questions per second` : 'Most recent completed bucket'} aria-label="Questions answered per second over time. Use arrow keys, Home and End to explore; Escape to dismiss." onpointermove={pointer} onpointerleave={() => { hover = null; }} onkeydown={key} onblur={() => { hover = null; }}>
    {#if !points.length}
      <text x={W / 2} y="150" text-anchor="middle" class="tick">{isLive ? 'Waiting for a completed second.' : 'History will appear after completed seconds.'}</text>
    {:else}
      {#each ticks as tick (tick)}
        <line x1="36" x2={W - 56} y1={y(tick)} y2={y(tick)} stroke={tick ? 'var(--grid)' : 'var(--axis)'} stroke-width="1" />
        <text x="28" y={y(tick) + 4} text-anchor="end" class="tick">{axisFormat.format(tick)}</text>
      {/each}
      {#each xTicks as tick (tick.t)}<text x={x(tick.i)} y="294" text-anchor="middle" class="tick">{timeFormat.format(tick.t)}</text>{/each}
      <path d={`${path}L${x(last)},${y(0)}L${x(0)},${y(0)}Z`} fill={isLive ? 'var(--deemph)' : 'var(--series-1)'} fill-opacity="0.1" />
      <path d={path} fill="none" stroke={isLive ? 'var(--deemph)' : 'var(--series-1)'} stroke-width="2" stroke-linejoin="round" stroke-linecap="round" />
      {#if isLive}<path d={average} fill="none" stroke="var(--series-1)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" />{/if}
      <circle cx={x(last)} cy={y(isLive ? points[last].average ?? points[last].v : points[last].v)} r="4" fill="var(--series-1)" stroke="var(--surface-1)" stroke-width="2" />
      <text x={x(last) + 8} y={y(isLive ? points[last].average ?? points[last].v : points[last].v) + 4} class="endlabel">{format.format(isLive ? points[last].average ?? points[last].v : points[last].v)}</text>
      {#if focused && hover !== null}
        <line x1={x(hover)} x2={x(hover)} y1="10" y2="274" stroke="var(--axis)" stroke-width="1" />
        <circle cx={x(hover)} cy={y(focused.average ?? focused.v)} r="4" fill="var(--series-1)" stroke="var(--surface-1)" stroke-width="2" />
        <g transform={`translate(${tooltipX},${tooltipY})`} class="svg-tooltip">
          <rect width="250" height={focused.provisional || isLive ? 84 : 62} rx="10" fill="var(--surface-1)" stroke="var(--border)" />
          <text x="12" y="23" class="tooltip-value">{format.format(focused.v)} questions / s</text>
          <text x="12" y="43" class="tick">{new Date(focused.t).toLocaleString()}</text>
          {#if isLive}<text x="12" y="65" class="tick">10-second average: {format.format(focused.average ?? focused.v)}</text>
          {:else if focused.provisional}<text x="12" y="65" class="tick">{focused.incomplete ? 'so far · incomplete startup coverage' : `so far · ${focused.seconds} completed seconds`}</text>{/if}
        </g>
      {/if}
    {/if}
  </svg>
  <p class="sr-only" aria-live="polite">{focused ? `${new Date(focused.t).toLocaleString()}: ${format.format(focused.v)} questions per second. ${isLive ? `10-second average: ${format.format(focused.average ?? focused.v)}.` : pointNote(focused)}` : ''}</p>
</div>
