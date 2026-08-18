<script lang="ts">
  // The Sites tab. Top: the sites your other fleet machines expose, grouped
  // per machine, each mappable to a local port and then Open (browser) /
  // Copy (address) / Unmap. Bottom (collapsed by default): "Your ports" —
  // the services this machine is listening on, each exposable to the fleet
  // under a name (prefilled from the page <title> when there is one).
  import { app } from "../store.svelte";
  import { displayName, siteIcon, type ListeningService, type SiteAdvert } from "../types";

  // Whether the "Your ports" section is expanded — collapsed at first so the
  // fleet's sites lead.
  let portsOpen = $state(false);

  // Inline copy feedback: the row whose address was just copied flashes its
  // Copy button to "✓ Copied" for a beat, instead of firing a toast.
  let copiedSite = $state<string | null>(null);
  async function copyAddr(node: string, site: string, mapping: Parameters<typeof app.copySite>[0]) {
    if (await app.copySite(mapping)) {
      copiedSite = `${node}:${site}`;
      setTimeout(() => {
        if (copiedSite === `${node}:${site}`) copiedSite = null;
      }, 1400);
    }
  }

  // Per-row name drafts for the expose inputs, so typing a name doesn't
  // re-render away. Falls back to the advertised name, then the probed
  // default (page title) / classified name.
  let names = $state<Record<string, string>>({});
  function nameFor(svc: ListeningService): string {
    return names[svc.id] ?? (app.exposeName(svc.id) || app.defaultSiteName(svc));
  }
</script>

<div class="sites">
  <!-- Sites exposed by your other machines. -->
  <section class="block">
    <h5 class="block-head">Around your fleet</h5>
    {#if app.sitesByMachine.length === 0}
      <p class="hint faint">
        No sites from your other machines yet. Expose a service below, or on one of your other
        machines, to reach it here.
      </p>
    {:else}
      {#each app.sitesByMachine as group (group.node.id)}
        <div class="machine">
          <div class="machine-head">
            <span class="m-dot" class:on={group.node.online}></span>
            {displayName(group.node)}
          </div>
          <ul class="list">
            {#each group.sites as site (site.id)}
              {@const mapping = app.siteMappingFor(group.node.id, site.id)}
              <li class="row">
                <span class="s-icon" aria-hidden="true">{siteIcon(site.scheme)}</span>
                <span class="s-main">
                  <span class="s-name">{site.label}<span class="s-port">:{site.port}</span></span>
                  {#if mapping}
                    <span class="s-sub mapped" title="Reachable locally at this address">
                      {app.siteUrl(mapping)}
                    </span>
                  {:else if site.loopback}
                    <span class="s-sub"><span class="tag">local-only</span></span>
                  {/if}
                </span>
                <div class="row-actions">
                  {#if mapping}
                    <button
                      class="btn small primary"
                      title="Open in your browser"
                      onclick={() => app.openSite(mapping)}>Open</button
                    >
                    <button
                      class="btn small ghost"
                      class:copied={copiedSite === `${group.node.id}:${site.id}`}
                      title="Copy its local address"
                      onclick={() => copyAddr(group.node.id, site.id, mapping)}
                      >{copiedSite === `${group.node.id}:${site.id}` ? "✓ Copied" : "Copy"}</button
                    >
                    <button
                      class="btn small ghost s-x"
                      title="Unmap: stop proxying it here"
                      aria-label="Unmap"
                      onclick={() => app.unmapSite(group.node.id, site.id)}>✕</button
                    >
                  {:else}
                    <button
                      class="btn small"
                      title="Map this site to a local port through the mesh proxy"
                      onclick={() => app.mapSite(group.node.id, site as SiteAdvert)}>Map</button
                    >
                  {/if}
                </div>
              </li>
            {/each}
          </ul>
        </div>
      {/each}
    {/if}
  </section>

  <!-- This machine's own exposed sites — always listed, even when their
       server has gone offline, so an exposed port is never just a count
       with nothing behind it. Open reaches the local service; Stop stops
       advertising it to the fleet; a red dot flags one that isn't
       responding. -->
  {#if app.myExposedSites.length > 0}
    <section class="block">
      <h5 class="block-head">Exposed from this machine</h5>
      <ul class="list">
        {#each app.myExposedSites as site (site.id)}
          <li class="row">
            <span class="s-icon" aria-hidden="true">{siteIcon(site.scheme)}</span>
            <span class="s-main">
              <span class="s-name">
                <span
                  class="svc-dot"
                  class:on={site.online}
                  title={site.online ? "Service is responding" : "Service isn't responding: it's offline"}
                ></span>
                {site.name}<span class="s-port">:{site.port}</span>
              </span>
              <span class="s-sub">
                {#if !site.online}<span class="tag bad">offline</span>{/if}
                {#if site.loopback}<span class="tag">local-only</span>{/if}
                {#if site.process}{site.process}{/if}
              </span>
            </span>
            <div class="row-actions">
              <button
                class="btn small primary"
                title="Open the local service in your browser"
                onclick={() => app.openLocalSite(site)}>Open</button
              >
              <button
                class="btn small ghost"
                title="Stop exposing it to your fleet"
                onclick={() => app.unexpose(site.id)}>Stop</button
              >
            </div>
          </li>
        {/each}
      </ul>
    </section>
  {/if}

  <!-- This machine's services — collapsed by default. -->
  <section class="block ports">
    <button class="ports-head" aria-expanded={portsOpen} onclick={() => (portsOpen = !portsOpen)}>
      <span class="ports-chevron" class:open={portsOpen} aria-hidden="true">▸</span>
      Your ports
      {#if Object.keys(app.exposedSites).length > 0}
        <span class="count">{Object.keys(app.exposedSites).length} exposed</span>
      {/if}
    </button>
    {#if portsOpen}
      {#if app.myListening.length === 0}
        <p class="hint faint">
          No listening services found{app.backendConnected ? "" : " (demo)"}: start a local
          server and it'll appear here to expose.
        </p>
      {:else}
        <ul class="list">
          {#each app.myListening as svc (svc.id)}
            {@const exposed = app.isExposed(svc.id)}
            <li class="row" class:lit={exposed}>
              <span class="s-icon" aria-hidden="true">{siteIcon(svc.scheme)}</span>
              <span class="s-main">
                <span class="s-name">{svc.name}<span class="s-port">:{svc.port}</span></span>
                <span class="s-sub">
                  {#if svc.loopback}<span class="tag">local-only</span>{/if}
                  {#if svc.process}{svc.process}{/if}
                </span>
              </span>
              <div class="row-actions">
                <input
                  class="name-in"
                  placeholder={app.defaultSiteName(svc)}
                  value={nameFor(svc)}
                  title="Name your fleet sees this site under"
                  oninput={(e) => (names[svc.id] = (e.currentTarget as HTMLInputElement).value)}
                  onkeydown={(e) => {
                    if (e.key === "Enter" && exposed) app.expose(svc.id, nameFor(svc));
                  }}
                  onblur={() => exposed && app.expose(svc.id, nameFor(svc))}
                />
                {#if exposed}
                  <button
                    class="btn small ghost"
                    title="Stop advertising this service"
                    onclick={() => app.unexpose(svc.id)}>Stop</button
                  >
                {:else}
                  <button
                    class="btn small primary"
                    title="Advertise this service to your fleet under this name"
                    onclick={() => app.expose(svc.id, nameFor(svc))}>Expose</button
                  >
                {/if}
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    {/if}
  </section>
</div>

<style>
  .sites {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
  }
  /* The transient "✓ Copied" state on a row's Copy button. */
  .btn.copied {
    color: var(--ok);
    border-color: color-mix(in oklab, var(--ok) 45%, transparent);
    background: color-mix(in oklab, var(--ok) 14%, transparent);
  }
  .block-head {
    margin: 0 0 0.4rem;
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0;
    line-height: 1.4;
  }
  .hint.faint {
    color: var(--ink-faint);
  }
  .machine {
    margin-bottom: 0.5rem;
  }
  .machine-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.78rem;
    font-weight: 600;
    color: var(--ink-soft);
    margin-bottom: 0.3rem;
  }
  .m-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .m-dot.on {
    background: var(--ok);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.35rem 0.4rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.4rem 0.4rem 0.5rem;
  }
  .row.lit {
    box-shadow: inset 0 0 0 1px var(--accent);
  }
  .s-icon {
    font-size: 0.95rem;
    flex-shrink: 0;
  }
  .s-main {
    flex: 1 1 6rem;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  /* The actions cluster sits inline when the panel is wide and wraps to its
     own line when it's narrow (the min-width sidebar), so the controls never
     squash — "ports go multi-line". */
  .row-actions {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex: 1 1 10rem;
    min-width: 0;
  }
  .s-name {
    font-size: 0.82rem;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .s-port {
    color: var(--ink-faint);
    font-weight: 500;
    margin-left: 0.2rem;
    font-family: var(--mono);
    font-size: 0.76rem;
  }
  .s-sub {
    font-size: 0.68rem;
    color: var(--ink-faint);
    display: flex;
    align-items: center;
    gap: 0.3rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .s-sub.mapped {
    color: var(--accent-ink);
    font-family: var(--mono);
  }
  .tag {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0 0.32rem;
    font-size: 0.62rem;
    font-weight: 700;
    color: var(--ink-faint);
  }
  /* "offline" tag on an exposed site whose server isn't responding. */
  .tag.bad {
    color: var(--danger);
    border-color: color-mix(in oklab, var(--danger) 45%, transparent);
    background: color-mix(in oklab, var(--danger) 12%, transparent);
  }
  /* Per-site health dot for this machine's exposed sites: green when the
     service is responding, red when it's gone offline. */
  .svc-dot {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--danger);
    margin-right: 0.35rem;
    vertical-align: middle;
    flex-shrink: 0;
  }
  .svc-dot.on {
    background: var(--ok);
  }
  .s-x {
    flex-shrink: 0;
    padding: 0.25rem 0.4rem;
    color: var(--ink-faint);
  }
  .s-x:hover {
    color: var(--danger);
  }
  .name-in {
    flex: 1 1 4rem;
    min-width: 3.5rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.25rem 0.4rem;
    font-size: 0.74rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  .ports-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    width: 100%;
    border: none;
    background: transparent;
    color: var(--ink-soft);
    font: inherit;
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    cursor: pointer;
    padding: 0.2rem 0;
    margin-bottom: 0.3rem;
  }
  .ports-head:hover {
    color: var(--ink);
  }
  .ports-chevron {
    transition: transform 0.12s ease;
    font-size: 0.7rem;
  }
  .ports-chevron.open {
    transform: rotate(90deg);
  }
  .count {
    font-size: 0.62rem;
    font-weight: 700;
    text-transform: none;
    letter-spacing: 0;
    background: var(--accent-soft);
    color: var(--accent-ink);
    border-radius: var(--r-pill);
    padding: 0 0.32rem;
  }
</style>
