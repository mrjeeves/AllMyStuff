<script lang="ts">
  // The body of a dedicated terminal window (opened with `?terminal=<node>`):
  // boot the store, wait for the target machine to be *ready* — found on
  // the mesh, its AllMyStuff presence landed (which carries the terminal
  // feature), and the owner/fleet facts resolved — then hand the window to
  // the tabbed terminal. The main window never renders this — it opens
  // terminal windows instead of popovers.
  //
  // Readiness is staged, never a one-shot gate (the ConsoleHost rule):
  // this window's store boots from nothing and the facts arrive in order
  // (mesh node → presence → fleet roster, which is what proves a machine
  // is yours). A gate that fired on the first stage would refuse machines
  // that are perfectly yours a beat later.
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import { setWindowTitle, terminalAttachTarget } from "../tauri";
  import { displayName, isAppNode } from "../types";
  import Terminal from "./Terminal.svelte";
  import Toasts from "./Toasts.svelte";

  let { target }: { target: string } = $props();

  const node = $derived(app.machineByAnyId(target));

  // When this window was opened as a popped-out tab (`?attach=<session>`), its
  // first tab joins that shared shell instead of minting a fresh one. Read once
  // — it's fixed for the window's lifetime.
  const initialAttach = terminalAttachTarget();

  const stage = $derived.by(() => {
    const n = node;
    if (!n) return "finding" as const;
    if (!isAppNode(n)) return "presence" as const;
    if (!app.terminalSupported(n)) return "unsupported" as const;
    if (!app.terminalAllowed(n)) return "relationship" as const;
    return "ready" as const;
  });

  // Admission is a one-way gate for this window. The graph can briefly
  // re-key a peer between its bare pubkey and display id, and fleet/presence
  // facts can arrive on different polls. Re-mounting the terminal through
  // those harmless convergence blips mints a fresh unique terminal route on
  // every pass. Keep the already-authorized surface alive; the backend still
  // enforces every route and reports a real disconnect/rejection itself.
  let admitted = $state(false);
  $effect(() => {
    if (stage === "ready") admitted = true;
  });

  onMount(() => {
    void app.init();
  });

  $effect(() => {
    const n = node;
    if (n) void setWindowTitle(`${displayName(n)}: AllMyStuff terminal`);
  });
</script>

<div class="host">
  {#if admitted}
    <!-- `target` is fixed in this window's URL. Unlike `node.id`, it cannot
         oscillate while mesh identity forms converge and remount the shell. -->
    <Terminal host={target} windowed={true} {initialAttach} />
  {:else if stage === "unsupported"}
    <div class="notice">
      <div class="glyph">📟</div>
      <p><b>{displayName(node!)}</b> doesn't advertise terminal support :
        it's probably running an older AllMyStuff. Update it and this
        window will pick it up.</p>
    </div>
  {:else if stage === "relationship"}
    <div class="notice">
      <div class="glyph">🔑</div>
      <p><b>{displayName(node!)}</b> is here: resolving whether it's yours
        (fleet roster loading)… Terminals are <b>owner/fleet only</b>: if
        you haven't claimed this machine, do that from the graph first.</p>
    </div>
  {:else if stage === "presence"}
    <div class="notice">
      <div class="glyph">📡</div>
      <p><b>{displayName(node!)}</b> is on the mesh: waiting for its
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
    background: #14121f;
  }
  .notice {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.6rem;
    color: #9a93b8;
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
  .notice b {
    color: #d7d2ec;
  }
</style>
