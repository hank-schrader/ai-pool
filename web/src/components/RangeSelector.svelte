<script lang="ts">
  import type { Range } from '../lib/stats';
  let { range, select }: { range: Range; select: (range: Range) => void } = $props();
  const options: { value: Range; label: string }[] = [{ value: 0, label: 'Live' }, { value: 60, label: '1 hour' }, { value: 360, label: '6 hours' }, { value: 1440, label: '24 hours' }];
</script>
<div class="filters" role="group" aria-label="Time range">
  <div class="seg">
    {#each options as option (option.value)}<button type="button" aria-pressed={range === option.value} onclick={() => select(option.value)}>{option.label}</button>{/each}
  </div>
</div>
