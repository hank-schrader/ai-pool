<script lang="ts">
  import { format, formatInt, timeFormat, niceScale, alignedScale, tickFormatter, tokenTickFormatter, pointNote, type SeriesPoint } from '../lib/stats';
  let { points, isLive }: { points: SeriesPoint[]; isLive: boolean } = $props();
  let width = $state(960), hover = $state<number | null>(null);
  const W = $derived(Math.max(320, width));
  const scale = $derived(niceScale(Math.max(0, ...points.map(p => Math.max(p.v, p.average ?? 0))) * 1.05));
  const intervals = $derived(Math.round(scale.top / scale.step));
  const promptScale = $derived(alignedScale(Math.max(0, ...points.map(p => p.prompt ?? 0)) * 1.05, intervals));
  const axisFormat = $derived(tickFormatter(scale.step));
  const tokenFormat = $derived(tokenTickFormatter(promptScale.step));
  const ticks = $derived(Array.from({ length: intervals + 1 }, (_, i) => i));
  const rightWidth = $derived(Math.max(76, ...ticks.map(i => tokenFormat(i * promptScale.step).length * 7 + 16)));
  const plotWidth = $derived(W - 46 - rightWidth);
  const x = (i: number) => 46 + plotWidth * (points.length === 1 ? 1 : i / (points.length - 1));
  const y = (v: number) => 32 + 242 * (1 - v / scale.top);
  const promptY = (v: number) => 32 + 242 * (1 - v / promptScale.top);
  const path = $derived(points.map((p, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)},${y(p.v).toFixed(1)}`).join(''));
  const average = $derived(points.map((p, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)},${y(p.average ?? p.v).toFixed(1)}`).join(''));
  const promptPath = $derived.by(() => {
    let connected = false;
    return points.map((p, i) => {
      if (p.prompt == null) { connected = false; return ''; }
      const command = connected ? 'L' : 'M'; connected = true;
      return `${command}${x(i).toFixed(1)},${promptY(p.prompt).toFixed(1)}`;
    }).join('');
  });
  const promptDots = $derived(points.flatMap((p, i) => p.prompt != null && points[i - 1]?.prompt == null && points[i + 1]?.prompt == null ? [i] : []));
  const last = $derived(points.length - 1);
  const lastBlue = $derived(last >= 0 ? points[last].average ?? points[last].v : 0);
  const blueLabelY = $derived(Math.max(42, Math.min(266, y(lastBlue) + 4)));
  const promptLabelY = $derived.by(() => {
    const value = points[last]?.prompt;
    if (value == null) return 0;
    const target = Math.max(42, Math.min(266, promptY(value) + 4));
    return Math.abs(target - blueLabelY) >= 18 ? target : blueLabelY <= 248 ? blueLabelY + 18 : blueLabelY - 18;
  });
  const focused = $derived(hover === null ? undefined : points[hover]);
  const tooltipWidth = $derived(Math.min(300, W - 16));
  const tooltipX = $derived(Math.max(8, Math.min(W - tooltipWidth - 8, x(hover ?? 0) - tooltipWidth / 2)));
  const tooltipY = $derived(focused ? Math.max(32, Math.min(150, y(focused.average ?? focused.v) - 122)) : 32);
  const focusedText = $derived(focused ? `${new Date(focused.t).toLocaleString()}: ${format.format(focused.v)} questions per second. ${isLive ? `10-second average: ${format.format(focused.average ?? focused.v)} questions per second.` : pointNote(focused)} Average prompt size${isLive ? ', last 10 completed seconds' : ''}: ${focused.prompt == null ? 'unavailable; no answered requests or missing data' : `${formatInt.format(focused.prompt)} tokens per request`}.` : 'Most recent completed bucket');
  const xTicks = $derived.by(() => {
    const fit = Math.max(2, Math.min(6, Math.floor(plotWidth / 90)));
    const step = Math.max(1, Math.round(points.length / fit));
    return points.flatMap((p, i) => i % step === 0 ? [{ t: p.t, i }] : []);
  });
  function pointer(event: PointerEvent) {
    if (!points.length) return;
    const svg = event.currentTarget as SVGSVGElement, rect = svg.getBoundingClientRect();
    const position = (event.clientX - rect.left) * W / rect.width;
    hover = Math.max(0, Math.min(last, Math.round((position - 46) / plotWidth * last)));
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
<div class="legend" aria-label="Chart legend">
  {#if isLive}<span><i class="legend-second"></i>Per second</span>{/if}
  <span><i class="legend-average"></i>{isLive ? '10-second average' : 'Questions / s'}</span>
  <span><i class="legend-prompt"></i>Average prompt size (right axis)</span>
</div>
<div class="plot" bind:clientWidth={width}>
  <svg viewBox={`0 0 ${W} 300`} tabindex="0" role="slider" aria-roledescription="chart time cursor" aria-valuemin="0" aria-valuemax={Math.max(0, last)} aria-valuenow={hover ?? Math.max(0, last)} aria-valuetext={focusedText} aria-label="Questions answered per second (left axis) and average prompt size in tokens per request (right axis) over time. Both axes start at zero and scale independently. Prompt size uses the last 10 completed seconds in Live and each minute in history; gaps mean no answered requests or missing data. Use arrow keys, Home and End to explore; Escape to dismiss." onpointermove={pointer} onpointerleave={() => { hover = null; }} onkeydown={key} onblur={() => { hover = null; }}>
    <line x1="46" x2="58" y1="12" y2="12" stroke="var(--series-1)" stroke-width="2" />
    <text x="64" y="16" class="axis-title">questions / s</text>
    <line x1={W - 128} x2={W - 116} y1="12" y2="12" stroke="var(--series-prompt)" stroke-width="2" />
    <text x={W - 110} y="16" class="axis-title">tokens</text>
    {#if !points.length}
      <text x={W / 2} y="150" text-anchor="middle" class="tick">{isLive ? 'Waiting for a completed second.' : 'History will appear after completed seconds.'}</text>
    {:else}
      {#each ticks as tick (tick)}
        <line x1="46" x2={W - rightWidth} y1={y(tick * scale.step)} y2={y(tick * scale.step)} stroke={tick ? 'var(--grid)' : 'var(--axis)'} stroke-width="1" />
        <text x="38" y={y(tick * scale.step) + 4} text-anchor="end" class="tick">{axisFormat.format(tick * scale.step)}</text>
        <text x={W - rightWidth + 8} y={y(tick * scale.step) + 4} class="tick">{tokenFormat(tick * promptScale.step)}</text>
      {/each}
      {#each xTicks as tick (tick.t)}<text x={x(tick.i)} y="294" text-anchor="middle" class="tick">{timeFormat.format(tick.t)}</text>{/each}
      <path d={`${path}L${x(last)},${y(0)}L${x(0)},${y(0)}Z`} fill={isLive ? 'var(--deemph)' : 'var(--series-1)'} fill-opacity="0.1" />
      {#if isLive}<path d={path} fill="none" stroke="var(--deemph)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" />{/if}
      <path d={promptPath} fill="none" stroke="var(--series-prompt)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" />
      {#each promptDots as i (points[i].t)}<circle cx={x(i)} cy={promptY(points[i].prompt!)} r="2" fill="var(--series-prompt)" />{/each}
      <path d={isLive ? average : path} fill="none" stroke="var(--series-1)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" />
      <circle cx={x(last)} cy={y(lastBlue)} r="4" fill="var(--series-1)" stroke="var(--surface-1)" stroke-width="2" />
      <text x={x(last) - 8} y={blueLabelY} text-anchor="end" class="endlabel">{format.format(lastBlue)}</text>
      {#if points[last].prompt != null}
        <text x={x(last) - 8} y={promptLabelY} text-anchor="end" class="endlabel">{formatInt.format(points[last].prompt!)} tok</text>
      {/if}
      {#if focused && hover !== null}
        <line x1={x(hover)} x2={x(hover)} y1="32" y2="274" stroke="var(--axis)" stroke-width="1" />
        {#if focused.prompt != null}<circle cx={x(hover)} cy={promptY(focused.prompt)} r="3" fill="var(--series-prompt)" stroke="var(--surface-1)" stroke-width="2" />{/if}
        <circle cx={x(hover)} cy={y(focused.average ?? focused.v)} r="4" fill="var(--series-1)" stroke="var(--surface-1)" stroke-width="2" />
        <g transform={`translate(${tooltipX},${tooltipY})`} class="svg-tooltip">
          <rect width={tooltipWidth} height={focused.provisional || isLive ? 110 : 88} rx="10" fill="var(--surface-1)" stroke="var(--border)" />
          <text x="12" y="23" class="tooltip-value">{format.format(focused.v)} questions / s</text>
          <text x="12" y="43" class="tick">{new Date(focused.t).toLocaleString()}</text>
          <text x="12" y="65" class="tick">Average prompt: {focused.prompt == null ? '–' : `${formatInt.format(focused.prompt)} tok`}{isLive ? ' · 10 s' : ''}</text>
          {#if isLive}<text x="12" y="87" class="tick">10-second average: {format.format(focused.average ?? focused.v)} q/s</text>
          {:else if focused.provisional}<text x="12" y="87" class="tick">{focused.incomplete ? 'so far · incomplete startup coverage' : `so far · ${focused.seconds} completed seconds`}</text>{/if}
        </g>
      {/if}
    {/if}
  </svg>
  <p class="sr-only" aria-live="polite">{focused ? focusedText : ''}</p>
</div>
