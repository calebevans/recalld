<script lang="ts">
  import { page } from '$app/state';
  import Icon, { type IconName } from './Icon.svelte';
  import Mark from './Mark.svelte';
  import { scope } from '$lib/scope.svelte';
  import { prefs } from '$lib/prefs.svelte';
  import { session } from '$lib/session.svelte';
  import { namespacesQuery } from '$lib/queries';
  import { count } from '$lib/format';

  const views: { href: string; label: string; icon: IconName }[] = [
    { href: '/map', label: 'Memory map', icon: 'map' },
    { href: '/explore', label: 'Explore', icon: 'explore' },
    { href: '/timeline', label: 'Timeline', icon: 'timeline' },
    { href: '/health', label: 'Health', icon: 'health' },
  ];

  const namespaces = namespacesQuery();
  const total = $derived((namespaces.data ?? []).reduce((a, n) => a + n.memoryCount, 0));
  const current = (href: string) => page.url.pathname.startsWith(href);
  const status = $derived(
    session.status === 'connected'
      ? 'daemon connected'
      : session.status === 'loading'
        ? 'connecting…'
        : session.status === 'unsupported'
          ? 'daemon lacks UI support'
          : session.status === 'signedOut'
            ? 'signed out'
            : 'daemon unavailable',
  );
</script>

{#if prefs.sidebarCollapsed}
  <aside class="rail" aria-label="Sidebar">
    <button type="button" class="rail-btn mark-btn" aria-label="Expand sidebar" aria-expanded="false" onclick={() => prefs.toggleSidebar()}>
      <Mark size="var(--mark-rail)" />
    </button>
    <nav aria-label="Views" class="rail-nav">
      {#each views as v}
        <a href={v.href} class="rail-btn" class:active={current(v.href)} aria-current={current(v.href) ? 'page' : undefined} aria-label={v.label} title={v.label}>
          <Icon name={v.icon} />
        </a>
      {/each}
    </nav>
    <button type="button" class="rail-btn theme" aria-label="Dark theme" title="Dark theme" aria-pressed={prefs.theme === 'dark'} onclick={() => prefs.toggleTheme()}>
      <Icon name="moon" />
    </button>
  </aside>
{:else}
  <aside class="sidebar" aria-label="Sidebar">
    <div class="brand">
      <a class="wordmark" href="/map" aria-label="recalld: Memory map"><Mark /><span>recalld</span></a>
      <button type="button" class="icon-btn collapse" aria-label="Collapse sidebar" aria-expanded="true" onclick={() => prefs.toggleSidebar()}>
        <Icon name="collapse" stroke={1.8} />
      </button>
    </div>

    <nav aria-label="Views" class="views">
      {#each views as v}
        <a href={v.href} class:active={current(v.href)} aria-current={current(v.href) ? 'page' : undefined}>{v.label}</a>
      {/each}
    </nav>

    <div role="group" aria-label="Namespace scope" class="namespaces">
      <span class="micro-label heading">Namespaces</span>
      <button type="button" aria-pressed={scope.current === null} onclick={() => scope.set(null)}>
        <span>All namespaces</span><span class="mono n">{namespaces.data ? count(total) : ''}</span>
      </button>
      {#if namespaces.isPending}
        {#each [0, 1, 2] as i (i)}<span class="skeleton ns-skel"></span>{/each}
      {/if}
      {#each namespaces.data ?? [] as ns (ns.id)}
        <button type="button" aria-pressed={scope.current === ns.name} onclick={() => scope.set(ns.name)}>
          <span class="ns-name">{ns.name}</span><span class="mono n">{count(ns.memoryCount)}</span>
        </button>
      {/each}
    </div>

    <div class="foot">
      <span class="status mono" title="The UI never records access, so browsing does not change decay">read-only · {status}</span>
      <button type="button" class="icon-btn theme" aria-label="Dark theme" title="Dark theme" aria-pressed={prefs.theme === 'dark'} onclick={() => prefs.toggleTheme()}>
        <Icon name="moon" />
      </button>
    </div>
  </aside>
{/if}

<style>
  .sidebar {
    flex: 0 1 var(--sidebar-width);
    min-width: var(--sidebar-min);
    display: flex;
    flex-direction: column;
    gap: var(--space-8);
    padding: var(--space-6) var(--space-5);
    border-right: var(--hairline) solid var(--color-border);
    overflow-y: auto;
  }
  .brand {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .wordmark {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font: var(--weight-medium) var(--text-wordmark) var(--font-serif);
    letter-spacing: -0.01em;
    color: inherit;
    text-decoration: none;
  }
  .collapse {
    margin-right: calc(var(--space-2) * -1);
  }
  .views {
    display: flex;
    flex-direction: column;
    gap: calc(var(--space-1) / 2);
  }
  .views a {
    display: flex;
    align-items: center;
    height: var(--nav-item-h);
    padding: 0 calc(var(--space-2) + var(--space-1) / 2);
    margin: 0 calc((var(--space-2) + var(--space-1) / 2) * -1);
    border-radius: var(--radius-md);
    color: var(--color-text-2);
    text-decoration: none;
  }
  .views a:hover {
    background: var(--color-accent-soft);
  }
  .views a.active {
    background: var(--color-accent-soft);
    color: var(--color-accent-strong);
    font-weight: var(--weight-semibold);
  }
  .namespaces {
    display: flex;
    flex-direction: column;
    gap: calc(var(--space-1) / 2);
  }
  .heading {
    padding-bottom: calc(var(--space-1) * 1.5);
  }
  .namespaces button {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
    width: 100%;
    min-height: var(--ns-row-h);
    align-items: center;
    padding: 0;
    border: 0;
    background: transparent;
    text-align: left;
    cursor: pointer;
    color: var(--color-text-2);
  }
  .namespaces button[aria-pressed='true'] {
    color: var(--color-accent-strong);
    font-weight: var(--weight-semibold);
  }
  .ns-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .n {
    font-size: var(--text-value);
    font-weight: var(--weight-regular);
    color: var(--color-text-muted);
  }
  .ns-skel {
    height: var(--space-4);
    margin: var(--space-2) 0;
  }
  .foot {
    margin-top: auto;
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .status {
    font-size: var(--text-label);
    color: var(--color-text-muted);
  }
  .foot .theme {
    flex: none;
    margin: 0 calc(var(--space-2) * -1) calc(var(--space-2) * -1) 0;
  }
  .theme[aria-pressed='true'] {
    background: var(--color-accent-soft);
    color: var(--color-accent-strong);
  }
  .rail {
    flex: 0 0 var(--sidebar-rail);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-5);
    padding: var(--space-6) 0;
    border-right: var(--hairline) solid var(--color-border);
  }
  .rail-nav {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .rail-btn {
    width: var(--nav-item-h);
    height: var(--nav-item-h);
    border-radius: var(--radius-md);
    color: var(--color-text-2);
    display: flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: 0;
    cursor: pointer;
  }
  .mark-btn {
    border: var(--hairline) solid var(--color-border);
  }
  .rail-btn.active {
    background: var(--color-accent-soft);
    color: var(--color-accent-strong);
  }
  .rail .theme {
    margin-top: auto;
  }
  @media (max-width: 600px) {
    .sidebar,
    .rail {
      flex: 0 0 auto;
      flex-direction: row;
      flex-wrap: wrap;
      align-items: center;
      gap: var(--space-4);
      padding: var(--space-3) var(--space-4);
      border-right: 0;
      border-bottom: var(--hairline) solid var(--color-border);
      min-width: 0;
      width: 100%;
    }
    .views,
    .rail-nav {
      flex-direction: row;
    }
    .foot,
    .rail .theme {
      margin-top: 0;
    }
    .foot .theme {
      margin: 0;
    }
  }
</style>
