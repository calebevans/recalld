<!-- Stroke icons from the reference screens (24px grid, currentColor). -->
<script module lang="ts">
  export type IconName =
    | 'map' | 'explore' | 'timeline' | 'health' | 'collapse' | 'expand' | 'layers'
    | 'chevron' | 'close' | 'more' | 'plus' | 'minus' | 'fit' | 'search' | 'moon';
</script>

<script lang="ts">
  let { name, size = 'var(--icon-md)', stroke = 1.6 }: { name: IconName; size?: string; stroke?: number } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width={stroke}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === 'map'}
    <circle cx="6" cy="6" r="2.5" /><circle cx="18" cy="8" r="2.5" /><circle cx="10" cy="18" r="2.5" />
    <path d="M8.4 6.6l7.2 1M6.9 8.4l2.2 7.2M16.5 10.1l-5 5.9" />
  {:else if name === 'explore'}
    <path d="M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01" />
  {:else if name === 'timeline'}
    <path d="M3 12h18M7 8v8M12 5v14M17 9v6" />
  {:else if name === 'health'}
    <path d="M3 12h4l3-7 4 14 3-7h4" />
  {:else if name === 'collapse'}
    <path d="M15 6l-6 6 6 6" />
  {:else if name === 'expand'}
    <path d="M9 6l6 6-6 6" />
  {:else if name === 'layers'}
    <path d="M12 4l8 4-8 4-8-4 8-4z" /><path d="M4 12l8 4 8-4" /><path d="M4 16l8 4 8-4" />
  {:else if name === 'chevron'}
    <path d="M6 9l6 6 6-6" />
  {:else if name === 'close'}
    <path d="M6 6l12 12M18 6L6 18" />
  {:else if name === 'more'}
    <circle cx="5" cy="12" r="1.6" fill="currentColor" stroke="none" />
    <circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none" />
    <circle cx="19" cy="12" r="1.6" fill="currentColor" stroke="none" />
  {:else if name === 'plus'}
    <path d="M12 5v14M5 12h14" />
  {:else if name === 'minus'}
    <path d="M5 12h14" />
  {:else if name === 'fit'}
    <path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5" />
  {:else if name === 'search'}
    <circle cx="11" cy="11" r="7" /><path d="M20 20l-3.5-3.5" />
  {:else if name === 'moon'}
    <path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z" />
  {/if}
</svg>
