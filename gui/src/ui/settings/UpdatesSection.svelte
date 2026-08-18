<script lang="ts">
  // Updates pane: drives the `allmystuff-updater` (its own release feed, not
  // the daemon's): toggle auto-update, pick a channel + apply policy, see
  // what's staged, and check / apply on demand.
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { appVersion, isMobile, isTauri } from "../../tauri";

  const s = $derived(app.updateInfo);
  const pkg = $derived(s?.install_kind === "package_manager");
  const web = !isTauri();
  // The phone/tablet shell: the App Store owns updates there, so this pane
  // renders as a plain About: no updater controls, and no updater invokes
  // (the commands aren't registered in the mobile crate).
  const mobile = isMobile();
  // The running build's version, for the mobile About line. (On the phone
  // this reports the mobile crate's version.)
  let version = $state<string | null>(null);
  // The result of the last "Check now", shown inline here instead of as a toast.
  const checkResult = $derived(app.checkOutcomeText(app.updateOutcome));

  onMount(() => {
    if (mobile) void appVersion().then((v) => (version = v));
    else void app.loadUpdateStatus();
  });

  function fmtWhen(at: number | null | undefined): string {
    if (!at) return "never";
    const d = new Date(at * 1000);
    return d.toLocaleString();
  }

  function cleanVersion(v: string | null | undefined): string {
    return v?.replace(/^v/, "") || "Unknown";
  }

  function behind(current: string | null, pinned: string | null): boolean {
    if (!current || !pinned) return true;
    const have = cleanVersion(current).split(/[.-]/).slice(0, 3).map(Number);
    const want = cleanVersion(pinned).split(/[.-]/).slice(0, 3).map(Number);
    for (let i = 0; i < 3; i += 1) {
      if ((have[i] || 0) < (want[i] || 0)) return true;
      if ((have[i] || 0) > (want[i] || 0)) return false;
    }
    return false;
  }
</script>

<div class="section">
  <h3>{mobile ? "About" : "Updates"}</h3>

  {#if mobile}
    <section class="block head">
      <div>
        <div class="ver">AllMyStuff {#if version}<b>{version}</b>{/if}</div>
        <div class="hint">Updates are delivered through the App Store.</div>
      </div>
    </section>
  {:else if web}
    <p class="hint">Update controls are available in the desktop app.</p>
  {:else if !s}
    <p class="hint">Reading update status…</p>
  {:else}
    <!-- Version + install kind -->
    <section class="block head">
      <div>
        <div class="ver">Version <b>{s.current_version}</b></div>
        <div class="hint">
          {pkg ? "Installed via a package manager" : "Installed as a standalone binary"}
        </div>
      </div>
      <!-- Checking works everywhere, including a package-managed install: it
           can't install the update, but it can still tell you one is out. -->
      <button class="btn small" disabled={app.updateBusy} onclick={() => app.checkUpdates()}>
        {app.updateBusy ? "Checking…" : "Check now"}
      </button>
    </section>

    {#if checkResult && !app.updateBusy}
      <p class="check-result">{checkResult}</p>
    {/if}

    <section class="block components">
      <div class="component-intro">
        <b>Installed components</b>
        <span class="hint">Current and pinned versions for each running component.</span>
      </div>
      {#if app.componentVersions.length === 0}
        <p class="hint">Reading component versions…</p>
      {:else}
        <div class="component-list">
          {#each app.componentVersions as row (row.id)}
            <div class="component-row" class:stale={behind(row.current, row.pinned)}>
              <div class="component-copy">
                <div class="component-name">{row.label}</div>
                <div class="component-detail">{row.detail}</div>
                {#if row.installed && cleanVersion(row.installed) !== cleanVersion(row.current)}
                  <div class="component-disk">On disk: {cleanVersion(row.installed)}</div>
                {/if}
              </div>
              <div class="versions">
                <span><small>Current</small><b>{cleanVersion(row.current)}</b></span>
                <span><small>Pinned</small><b>{cleanVersion(row.pinned)}</b></span>
              </div>
              <button class="btn small repair" disabled={app.componentBusy !== null} onclick={() => app.repairComponent(row.id)}>
                {app.componentBusy === row.id ? "Working…" : behind(row.current, row.pinned) ? "Update" : "Repair"}
              </button>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    {#if pkg}
      <section class="block">
        <p class="notice">
          Update this copy with its package manager or original installer.
        </p>
      </section>
    {:else}
      <!-- Automatic updates -->
      <section class="block">
        <label class="toggle">
          <input
            type="checkbox"
            checked={s.enabled}
            onchange={(e) => app.setUpdatePrefs({ enabled: e.currentTarget.checked })}
          />
          <span>
            <b>Automatic updates</b>
            <span class="hint">Check the release feed in the background and stage what's permitted.</span>
          </span>
        </label>
      </section>

      <!-- Channel + policy + interval -->
      <section class="block grid">
        <label class="opt">
          <span class="opt-label">Channel</span>
          <select value={s.channel} onchange={(e) => app.setUpdatePrefs({ channel: e.currentTarget.value })}>
            <option value="stable">Stable</option>
            <option value="beta">Beta (pre-releases)</option>
          </select>
        </label>

        <label class="opt">
          <span class="opt-label">Auto-apply</span>
          <select value={s.auto_apply} onchange={(e) => app.setUpdatePrefs({ auto_apply: e.currentTarget.value })}>
            <option value="patch">Patch only (0.1.5 → 0.1.6)</option>
            <option value="minor">Up to minor (0.1 → 0.2)</option>
            <option value="all">Any version</option>
            <option value="none">None (stage, I apply)</option>
          </select>
        </label>

        <label class="opt">
          <span class="opt-label">Check every</span>
          <div class="interval">
            <input
              type="number"
              min="1"
              max="720"
              value={s.check_interval_hours}
              onchange={(e) => app.setUpdatePrefs({ check_interval_hours: Math.max(1, Number(e.currentTarget.value) || 24) })}
            />
            <span class="unit">hours</span>
          </div>
        </label>
      </section>

      <!-- Staged / applied update -->
      {#if app.updateApplied}
        <section class="block staged">
          <div>
            <div class="ver"><b>{app.updateApplied}</b> is ready</div>
            <div class="hint">Relaunch AllMyStuff to start running the new version.</div>
          </div>
          <button class="btn small primary" disabled={app.updateBusy} onclick={() => app.relaunchUpdate()}>
            {app.updateBusy ? "Relaunching…" : "Relaunch now"}
          </button>
        </section>
      {:else if s.staged_version}
        <section class="block staged">
          <div>
            <div class="ver"><b>{s.staged_version}</b> is downloaded</div>
            <div class="hint">Relaunch to update now, or it applies on its own next launch.</div>
          </div>
          <div class="staged-actions">
            <button class="btn small" disabled={app.updateBusy} onclick={() => app.applyUpdate()}>
              Apply only
            </button>
            <button class="btn small primary" disabled={app.updateBusy} onclick={() => app.relaunchUpdate()}>
              {app.updateBusy ? "Relaunching…" : "Relaunch & update"}
            </button>
          </div>
        </section>
      {/if}

      <!-- Feed + last check -->
      <section class="block info">
        <div class="info-row"><span>Last checked</span><b>{fmtWhen(s.last_check_at)}</b></div>
        <div class="info-row">
          <span>Release feed</span>
          <b class="feed" title={s.release_url}>
            {s.release_url_overridden ? "custom" : "default"}
          </b>
        </div>
      </section>
    {/if}
  {/if}

</div>

<style>
  .section {
    display: flex;
    flex-direction: column;
  }
  h3 {
    margin: 0 0 0.4rem;
    font-size: 1.2rem;
  }
  /* Inline result of the last "Check now": replaces the old toast. */
  .check-result {
    font-size: 0.82rem;
    font-weight: 600;
    color: var(--ink-soft);
    margin: 0.5rem 0 0;
    padding: 0.45rem 0.65rem;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0.15rem 0 0;
    line-height: 1.45;
    display: block;
  }
  .block {
    border-top: 1px solid var(--line);
    padding: 0.9rem 0;
  }
  .block:first-of-type {
    border-top: none;
    padding-top: 0.2rem;
  }
  .head,
  .staged {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .ver {
    font-size: 0.95rem;
  }
  .notice {
    font-size: 0.82rem;
    color: var(--ink-soft);
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.6rem 0.7rem;
    margin: 0;
    line-height: 1.45;
  }
  .toggle {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    cursor: pointer;
  }
  .toggle input {
    margin-top: 0.2rem;
  }
  .grid {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
  }
  .opt {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .opt-label {
    font-size: 0.86rem;
    font-weight: 550;
  }
  select,
  input[type="number"] {
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.55rem;
    font-size: 0.84rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  select {
    min-width: 14rem;
  }
  .interval {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  input[type="number"] {
    width: 5rem;
  }
  .unit {
    font-size: 0.8rem;
    color: var(--ink-faint);
  }
  .staged {
    background: var(--accent-soft);
    border-radius: var(--r-sm);
    border-top: none;
    padding: 0.7rem 0.8rem;
    margin-top: 0.3rem;
  }
  .staged-actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .info {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .info-row {
    display: flex;
    justify-content: space-between;
    font-size: 0.82rem;
    color: var(--ink-soft);
  }
  .feed {
    text-transform: capitalize;
  }

  .components { display: flex; flex-direction: column; gap: 0.65rem; }
  .component-intro { display: flex; flex-direction: column; }
  .component-list { border: 1px solid var(--line); border-radius: var(--r-sm); overflow: hidden; }
  .component-row { display: grid; grid-template-columns: minmax(10rem, 1fr) auto auto; align-items: center; gap: 0.8rem; padding: 0.65rem 0.7rem; border-top: 1px solid var(--line); }
  .component-row:first-child { border-top: none; }
  .component-row.stale { background: color-mix(in srgb, var(--danger-soft) 50%, transparent); }
  .component-copy { min-width: 0; }
  .component-name { font-size: 0.84rem; font-weight: 650; }
  .component-detail, .component-disk { color: var(--ink-faint); font-size: 0.7rem; margin-top: 0.1rem; }
  .versions { display: flex; gap: 0.8rem; }
  .versions span { min-width: 4.2rem; display: flex; flex-direction: column; }
  .versions small { color: var(--ink-faint); font-size: 0.62rem; text-transform: uppercase; letter-spacing: 0.04em; }
  .versions b { font-size: 0.78rem; }
  .repair { min-width: 4.3rem; }
  @media (max-width: 700px) {
    .component-row { grid-template-columns: 1fr auto; }
    .versions { grid-column: 1; }
    .repair { grid-column: 2; grid-row: 1 / span 2; }
  }
</style>
