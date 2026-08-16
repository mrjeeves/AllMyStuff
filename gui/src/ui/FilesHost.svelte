<script lang="ts">
  // The body of a dedicated files window (opened with `?files=<node>`):
  // boot the store, wait for the target machine to be *ready* — found on
  // the mesh, its AllMyStuff presence landed (which carries the files
  // feature), and the owner/fleet facts resolved — then hand the window
  // to the file manager. Staged readiness, never a one-shot gate (the
  // TerminalHost rule): this window's store boots from nothing and the
  // facts arrive in order; a gate that fired on the first stage would
  // refuse machines that are perfectly yours a beat later.
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import { setWindowTitle } from "../tauri";
  import { displayName, isAppNode } from "../types";
  import Files from "./Files.svelte";
  import Toasts from "./Toasts.svelte";

  let { target }: { target: string } = $props();

  const node = $derived(app.machineByAnyId(target));

  const stage = $derived.by(() => {
    const n = node;
    if (!n) return "finding" as const;
    if (!isAppNode(n)) return "presence" as const;
    if (!app.filesSupported(n)) return "unsupported" as const;
    if (!app.filesAllowed(n)) return "relationship" as const;
    return "ready" as const;
  });

  // Once this window has been admitted, keep its route-owning child mounted.
  // Presence and fleet facts converge independently and a node can be re-keyed
  // between bare/display ids; treating those transient forms as a new child
  // leaked one unique Files route per remount.
  let admitted = $state(false);
  $effect(() => {
    if (stage === "ready") admitted = true;
  });

  onMount(() => {
    void app.init();
  });

  $effect(() => {
    const n = node;
    if (n) void setWindowTitle(`${displayName(n)} — AllMyStuff files`);
  });
</script>

<div class="host">
  {#if admitted}
    <Files host={target} windowed={true} />
  {:else if stage === "unsupported"}
    <div class="notice">
      <div class="glyph">🗂</div>
      <p><b>{displayName(node!)}</b> doesn't advertise file browsing —
        it's probably running an older AllMyStuff. Update it and this
        window will pick it up.</p>
    </div>
  {:else if stage === "relationship"}
    <div class="notice">
      <div class="glyph">🔑</div>
      <p><b>{displayName(node!)}</b> is here — resolving whether it's yours
        (fleet roster loading)… Files are <b>owner/fleet only</b>: if you
        haven't claimed this machine, do that from the graph first.</p>
    </div>
  {:else if stage === "presence"}
    <div class="notice">
      <div class="glyph">📡</div>
      <p><b>{displayName(node!)}</b> is on the mesh — waiting for its
        AllMyStuff presence (its features and ownership) to land…</p>
    </div>
  {:else}
    <div class="notice">
      <div class="glyph">📡</div>
      <p>Finding this machine on the mesh…</p>
    </div>
  {/if}
  <Toasts />
</div>

<style>
  .host {
    height: 100vh;
    background: var(--surface, #fff);
  }
  .notice {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.6rem;
    color: var(--ink-soft, #5a5470);
    text-align: center;
    padding: 2rem;
  }
  .glyph {
    font-size: 2.6rem;
  }
  .notice p {
    max-width: 26rem;
    font-size: 0.9rem;
    line-height: 1.5;
  }
</style>
