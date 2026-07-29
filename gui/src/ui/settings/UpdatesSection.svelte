<script lang="ts">
  // Updates pane — drives the `allmystuff-updater` (its own release feed, not
  // the daemon's): toggle auto-update, pick a channel + apply policy, see
  // what's staged, and check / apply on demand.
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { appVersion, isMobile, isTauri } from "../../tauri";

  const s = $derived(app.updateInfo);
  const pkg = $derived(s?.install_kind === "package_manager");
  const web = !isTauri();
  // The phone/tablet shell: the App Store owns updates there, so this pane
  // renders as a plain About — no updater controls, and no updater invokes
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

  // ---- Danger Zone ----
  // Two-click arming so a reset can't fire on a stray click. Each level reboots
  // the whole stack (node + daemon + app) after clearing state: the daemon is
  // the real datastore, so without the reboot an in-memory cache can re-persist
  // ("resurrect") what was wiped. Desktop only — it needs the local daemon and
  // a relaunch.
  let armed = $state<null | "leave" | "network" | "factory">(null);
  let resetting = $state<null | "leave" | "network" | "factory">(null);
  let resetError = $state<string | null>(null);

  async function runReset(kind: "leave" | "network" | "factory") {
    if (armed !== kind) {
      armed = kind; // first click arms; a second confirms
      return;
    }
    armed = null;
    resetting = kind;
    resetError = null;
    try {
      if (kind === "leave") await app.dangerLeaveFleet();
      else if (kind === "network") await app.dangerResetNetworking();
      else await app.dangerFactoryReset();
      // The app relaunches on success, so control doesn't normally return here.
    } catch (e) {
      resetError = String(e);
      resetting = null;
    }
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
    <p class="hint">Update controls appear in the desktop app — this is the in-browser preview.</p>
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

    {#if pkg}
      <section class="block">
        <p class="notice">
          This copy of AllMyStuff can't replace its own files — it was installed
          through a package manager (Homebrew, apt, Chocolatey, Scoop), or into a
          location that needs an administrator to write to. It still checks for
          new releases and will tell you when one lands; installing it is done
          the same way you installed AllMyStuff.
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

  <!-- Danger Zone — desktop only (it needs the local daemon and a relaunch).
       Each action reboots the node + daemon + app so cleared state genuinely
       flushes instead of being resurrected from an in-memory cache. -->
  {#if !web && !mobile}
    <section class="danger">
      <div class="danger-head">⚠ Danger Zone</div>
      <p class="danger-lead">
        Each of these clears state and then <b>restarts the app and the mesh
        daemon</b>, so every layer reloads from disk. Without the reboot, cached
        state can quietly reappear.
      </p>

      <div class="danger-row">
        <div class="danger-copy">
          <div class="danger-title">Leave the fleet</div>
          <div class="danger-desc">
            Remove this device from its fleet — drops ownership, the fleet key,
            and the fleet's signed roster. Keeps your other meshes and settings.
          </div>
        </div>
        <button
          class="danger-btn"
          class:armed={armed === "leave"}
          disabled={resetting !== null}
          onclick={() => runReset("leave")}
        >
          {resetting === "leave"
            ? "Restarting…"
            : armed === "leave"
              ? "Confirm — reboots"
              : "Leave fleet"}
        </button>
      </div>

      <div class="danger-row">
        <div class="danger-copy">
          <div class="danger-title">Reset networking</div>
          <div class="danger-desc">
            Leave the fleet <b>and</b> forget every mesh — all rosters and signed
            state — keeping this device's identity. A clean networking slate.
          </div>
        </div>
        <button
          class="danger-btn"
          class:armed={armed === "network"}
          disabled={resetting !== null}
          onclick={() => runReset("network")}
        >
          {resetting === "network"
            ? "Restarting…"
            : armed === "network"
              ? "Confirm — reboots"
              : "Reset networking"}
        </button>
      </div>

      <div class="danger-row">
        <div class="danger-copy">
          <div class="danger-title">Factory reset</div>
          <div class="danger-desc">
            Erase <b>everything</b> — identity, config, every mesh, and fleet
            ownership. This device becomes brand-new to all peers. No undo.
          </div>
        </div>
        <button
          class="danger-btn nuke"
          class:armed={armed === "factory"}
          disabled={resetting !== null}
          onclick={() => runReset("factory")}
        >
          {resetting === "factory"
            ? "Resetting…"
            : armed === "factory"
              ? "Confirm wipe — reboots"
              : "Factory reset"}
        </button>
      </div>

      {#if armed}
        <button class="danger-cancel" onclick={() => (armed = null)}>Cancel</button>
      {/if}
      {#if resetError}
        <p class="danger-err">Reset failed: {resetError}</p>
      {/if}
    </section>
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
  /* Inline result of the last "Check now" — replaces the old toast. */
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

  /* ---- Danger Zone ---- */
  .danger {
    margin-top: 1.4rem;
    padding: 0.9rem;
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    background: var(--danger-soft);
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
  }
  .danger-head {
    color: var(--danger);
    font-weight: 700;
    font-size: 0.9rem;
    letter-spacing: 0.02em;
  }
  .danger-lead {
    color: var(--ink-soft);
    font-size: 0.78rem;
    margin: 0;
    line-height: 1.4;
  }
  .danger-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.8rem;
    padding-top: 0.6rem;
    border-top: 1px solid var(--line);
  }
  .danger-copy {
    min-width: 0;
  }
  .danger-title {
    color: var(--ink);
    font-size: 0.85rem;
    font-weight: 600;
  }
  .danger-desc {
    color: var(--ink-soft);
    font-size: 0.76rem;
    line-height: 1.4;
    margin-top: 0.15rem;
  }
  .danger-btn {
    flex: 0 0 auto;
    white-space: nowrap;
    background: var(--danger-soft);
    color: var(--danger);
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.7rem;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .danger-btn:hover:not(:disabled) {
    filter: brightness(1.15);
  }
  .danger-btn.armed {
    background: var(--danger);
    color: #fff;
    font-weight: 600;
  }
  .danger-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .danger-cancel {
    align-self: flex-start;
    background: none;
    border: none;
    color: var(--ink-faint);
    font-size: 0.76rem;
    cursor: pointer;
    text-decoration: underline;
    padding: 0;
  }
  .danger-err {
    color: var(--danger);
    background: var(--danger-soft);
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    padding: 0.45rem 0.6rem;
    font-size: 0.8rem;
    margin: 0;
  }
</style>
