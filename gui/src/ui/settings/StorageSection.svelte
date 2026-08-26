<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { humanBytes, type MeshNode, type StorageSummary } from "../../types";
  import {
    fleetStorageLocalVolumes,
    fleetStorageSetAllocation,
    fleetStorageSetPolicy,
    onFleetStorage,
    fleetStorageStatus,
    type FleetStoragePolicy,
    type FleetStorageStatus,
    type FleetStorageVolume,
  } from "../../tauri";

  let status = $state<FleetStorageStatus | null>(null);
  let localVolumes = $state<FleetStorageVolume[]>([]);
  let loading = $state(true);
  let error = $state("");
  let saving = $state<string | null>(null);

  const nodes = $derived(app.catalog.nodes.filter((node) => app.isMe(node.id) || app.isFleetMember(node.id)));
  const policy = $derived(status?.plan.policy.value);
  const allocations = $derived(status?.plan.allocations ?? []);
  const rawAllocated = $derived(allocations.filter((item) => item.enabled).reduce((sum, item) => sum + item.quotaBytes, 0));
  const protectedCapacity = $derived(policy ? Math.floor(rawAllocated * (1 - policy.reservePercent / 100) / policy.ordinaryReplicas) : 0);
  const activeResources = $derived(allocations.filter((item) => item.enabled && resourceFor(item.device, item.volume)?.available_bytes).length);

  onMount(() => {
    let stop = () => {};
    void refresh();
    void onFleetStorage((plan) => {
      if (status) status = { ...status, plan };
      else void refresh();
    }).then((unlisten) => { stop = unlisten; });
    return () => stop();
  });

  async function refresh() {
    loading = true;
    error = "";
    try {
      [status, localVolumes] = await Promise.all([fleetStorageStatus(), fleetStorageLocalVolumes()]);
    } catch (cause) {
      error = String(cause);
    } finally {
      loading = false;
    }
  }

  function volumesFor(node: MeshNode): StorageSummary[] {
    if (app.isMe(node.id) && localVolumes.length) {
      return localVolumes.map((volume) => ({
        id: volume.id,
        name: volume.name,
        total_bytes: volume.totalBytes,
        available_bytes: volume.availableBytes,
        removable: volume.removable,
        kind: volume.kind,
      }));
    }
    return node.summary?.storage ?? [];
  }

  function nodeFor(device: string): MeshNode | undefined {
    return app.machineByAnyId(device);
  }

  function resourceFor(device: string, volume: string): StorageSummary | undefined {
    const node = nodeFor(device);
    return node && volumesFor(node).find((candidate) => candidate.id === volume);
  }

  function allocationFor(node: MeshNode, volume: StorageSummary) {
    return allocations.find((allocation) => nodeFor(allocation.device)?.id === node.id && allocation.volume === volume.id);
  }

  async function setAllocation(node: MeshNode, volume: StorageSummary, enabled: boolean, quotaBytes?: number) {
    const key = node.id + ":" + volume.id;
    saving = key;
    error = "";
    try {
      const current = allocationFor(node, volume);
      const quota = Math.max(1, Math.min(volume.total_bytes, quotaBytes ?? current?.quotaBytes ?? volume.available_bytes));
      await fleetStorageSetAllocation(node.id, volume.id, quota, enabled);
      status = await fleetStorageStatus();
    } catch (cause) {
      error = String(cause);
    } finally {
      saving = null;
    }
  }

  async function updatePolicy(patch: Partial<FleetStoragePolicy>) {
    if (!policy) return;
    saving = "policy";
    error = "";
    try {
      await fleetStorageSetPolicy({ ...policy, ...patch });
      status = await fleetStorageStatus();
    } catch (cause) {
      error = String(cause);
    } finally {
      saving = null;
    }
  }

  function profileFor(node: MeshNode) {
    return status?.profiles.find((profile) => nodeFor(profile.peer)?.id === node.id);
  }
</script>

<section class="storage">
  <div class="heading">
    <div>
      <h4>Fleetfiles storage</h4>
      <p>Allocate capacity once. AllMyStuff places, verifies, and repairs managed data across the fleet.</p>
    </div>
    <button class="quiet" onclick={refresh} disabled={loading} title="Refresh device capacity">↻</button>
  </div>

  {#if loading}
    <p class="muted">Reading fleet storage…</p>
  {:else if status && policy}
    <div class="capacity">
      <div><span>Protected capacity</span><strong>{humanBytes(protectedCapacity)}</strong><small>after {policy.ordinaryReplicas} copies and {policy.reservePercent}% reserve</small></div>
      <div><span>Allocated physical</span><strong>{humanBytes(rawAllocated)}</strong><small>{activeResources} active resource{activeResources === 1 ? "" : "s"}</small></div>
      <div><span>Protection</span><strong>{activeResources >= policy.ordinaryReplicas ? "Ready" : "Needs capacity"}</strong><small>{activeResources}/{policy.ordinaryReplicas} ordinary targets</small></div>
    </div>

    <div class="policy">
      <label>
        <span>Ordinary copies</span>
        <select value={policy.ordinaryReplicas} disabled={saving === "policy"} onchange={(event) => {
          const ordinaryReplicas = Number(event.currentTarget.value);
          void updatePolicy({ ordinaryReplicas, criticalReplicas: Math.max(ordinaryReplicas, policy.criticalReplicas) });
        }}>
          {#each [1, 2, 3, 4, 5] as count}<option value={count}>{count}</option>{/each}
        </select>
      </label>
      <label>
        <span>Critical copies</span>
        <select value={policy.criticalReplicas} disabled={saving === "policy"} onchange={(event) => void updatePolicy({ criticalReplicas: Number(event.currentTarget.value) })}>
          {#each [policy.ordinaryReplicas, 3, 4, 5, 6].filter((value, index, values) => value >= policy.ordinaryReplicas && values.indexOf(value) === index) as count}
            <option value={count}>{count}</option>
          {/each}
        </select>
      </label>
      <label>
        <span>Free-space reserve</span>
        <select value={policy.reservePercent} disabled={saving === "policy"} onchange={(event) => void updatePolicy({ reservePercent: Number(event.currentTarget.value) })}>
          {#each [5, 10, 15, 20, 30] as reserve}<option value={reserve}>{reserve}%</option>{/each}
        </select>
      </label>
      <label class="metered"><input type="checkbox" checked={policy.pauseOnMetered} disabled={saving === "policy"} onchange={(event) => void updatePolicy({ pauseOnMetered: event.currentTarget.checked })} /><span>Pause maintenance on metered networks</span></label>
    </div>

    <div class="resources">
      <div class="resources-head"><b>Storage resources</b><span>Native paths remain private.</span></div>
      {#each nodes as node (node.id)}
        {@const volumes = volumesFor(node)}
        {@const profile = profileFor(node)}
        <article class="device">
          <header>
            <div><b>{node.label}</b><small>{app.isMe(node.id) ? "This device" : node.online ? "Online" : "Offline"}</small></div>
            {#if profile}<span class="candidate" title={Math.round(profile.confidence * 100) + "% scheduling confidence"}>{Math.round(profile.serviceScore * 100)} candidate</span>{/if}
          </header>
          {#if volumes.length === 0}
            <p class="muted">Capacity has not been advertised by this device yet.</p>
          {:else}
            {#each volumes as volume (volume.id)}
              {@const allocation = allocationFor(node, volume)}
              {@const key = node.id + ":" + volume.id}
              <div class="volume">
                <label class="enable">
                  <input type="checkbox" checked={allocation?.enabled ?? false} disabled={saving === key} onchange={(event) => void setAllocation(node, volume, event.currentTarget.checked)} />
                  <span><b>{volume.name || volume.id}</b><small>{volume.kind.toUpperCase()}{volume.removable ? " · removable" : ""}</small></span>
                </label>
                <div class="space"><span>{humanBytes(volume.available_bytes)} free of {humanBytes(volume.total_bytes)}</span><progress max={Math.max(1, volume.total_bytes)} value={Math.max(0, volume.total_bytes - volume.available_bytes)}></progress></div>
                {#if allocation?.enabled}
                  <label class="quota">
                    <span>Contribute {humanBytes(allocation.quotaBytes)} ({Math.round(allocation.quotaBytes / Math.max(1, volume.total_bytes) * 100)}%)</span>
                    <input type="range" min={Math.min(1_073_741_824, volume.total_bytes)} max={Math.max(1, volume.total_bytes)} step={Math.max(1, Math.floor(volume.total_bytes / 100))} value={allocation.quotaBytes} disabled={saving === key} onchange={(event) => void setAllocation(node, volume, true, event.currentTarget.valueAsNumber)} />
                  </label>
                {/if}
              </div>
            {/each}
          {/if}
        </article>
      {/each}
    </div>
  {/if}

  {#if error}<p class="error">{error}</p>{/if}
  <p class="footnote">Candidate ratings use bounded observations from real fleet operations. Unknown history lowers confidence; it is not treated as perfect uptime.</p>
</section>

<style>
  .storage { display: grid; gap: .8rem; padding: .9rem; border: 1px solid var(--line); border-radius: var(--r-sm); background: var(--surface-2); }
  .heading, .resources-head, .device header, .volume { display: flex; align-items: center; justify-content: space-between; gap: .8rem; }
  h4, p { margin: 0; } h4 { font-size: .92rem; }
  .heading p, .muted, .footnote { color: var(--ink-faint); font-size: .74rem; line-height: 1.45; }
  .quiet { border: 1px solid var(--line); background: var(--surface); color: var(--ink-soft); border-radius: 7px; width: 2rem; height: 2rem; }
  .capacity { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: .45rem; }
  .capacity > div { display: grid; gap: .12rem; padding: .65rem; border-radius: 8px; background: var(--bg); }
  .capacity span, .capacity small { color: var(--ink-faint); font-size: .68rem; } .capacity strong { font-size: 1rem; }
  .policy { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: .5rem; }
  .policy label { display: grid; gap: .25rem; color: var(--ink-soft); font-size: .72rem; }
  .policy select { width: 100%; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); color: var(--ink); padding: .35rem; }
  .policy .metered { grid-column: 1 / -1; display: flex; align-items: center; }
  .resources { display: grid; gap: .5rem; } .resources-head span { color: var(--ink-faint); font-size: .68rem; }
  .device { display: grid; gap: .45rem; padding: .65rem; border: 1px solid var(--line); border-radius: 8px; background: var(--surface); }
  .device header > div, .enable span { display: grid; gap: .08rem; } .device small { color: var(--ink-faint); font-size: .66rem; }
  .candidate { padding: .2rem .4rem; border-radius: 99px; background: var(--accent-soft); color: var(--accent-ink); font-size: .65rem; }
  .volume { align-items: start; padding-top: .45rem; border-top: 1px solid var(--line); flex-wrap: wrap; }
  .enable { display: flex; align-items: start; gap: .45rem; min-width: 11rem; } .space { display: grid; gap: .2rem; min-width: 11rem; color: var(--ink-soft); font-size: .68rem; }
  progress { width: 100%; height: .38rem; } .quota { display: grid; gap: .25rem; flex-basis: 100%; color: var(--ink-soft); font-size: .68rem; padding-left: 1.45rem; } .quota input { width: 100%; }
  .error { color: var(--danger); font-size: .75rem; } .footnote { border-top: 1px solid var(--line); padding-top: .55rem; }
  @media (max-width: 700px) { .capacity, .policy { grid-template-columns: 1fr; } .policy .metered { grid-column: auto; } }
</style>

