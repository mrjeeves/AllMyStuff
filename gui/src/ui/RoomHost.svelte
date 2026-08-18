<script lang="ts">
  // The body of a dedicated room window (opened with `?room=<room id>`):
  // boot the store, wait for the room to surface (the saved list loads
  // instantly; an invite that's still in flight lands via the rooms
  // plane), then join the call *in this window* and keep the OS window
  // title naming the room. The main window never renders this — on the
  // desktop it opens room windows instead of the in-page overlay, so the
  // call can be moved, resized and full-screened like any console.
  //
  // Closing the window is hanging up: the close is held while the room's
  // legs tear down and members hear the leave, then the window finishes
  // the job itself. The reverse is also true — when this window stops
  // being in the room (you left, the host removed this device, the host
  // closed the room), the window closes.
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import { closeThisWindow, isTauri, onThisWindowClose, setWindowTitle } from "../tauri";
  import RoomPanel from "./RoomPanel.svelte";
  import Toasts from "./Toasts.svelte";

  let { target }: { target: string } = $props();

  let joined = $state(false);
  let closing = false;
  const room = $derived(app.rooms.find((r) => r.id === target));
  // Join only once this window's store knows who it is — the saved rooms
  // list loads instantly, but the device id lands with the first scan,
  // and a join broadcast before that would stamp presence with the
  // placeholder id. (The web preview's id *is* the placeholder.)
  const identityReady = $derived(!isTauri() || app.localId !== "this");

  async function hangUp() {
    if (closing) return;
    closing = true;
    app.leaveRoom(target);
    // The leave broadcast is fire-and-forget; one beat lets it reach the
    // backend before the webview dies. Bounded — a wedged daemon must
    // never hold a closing window hostage.
    await new Promise((r) => setTimeout(r, 250));
    void closeThisWindow();
  }

  onMount(() => {
    void app.init();
    let unlisten: (() => void) | null = null;
    void onThisWindowClose(() => void hangUp()).then((u) => (unlisten = u));
    return () => unlisten?.();
  });

  $effect(() => {
    if (joined || !room || !identityReady) return;
    joined = true;
    app.joinRoomHere(room.id);
  });

  $effect(() => {
    if (room) void setWindowTitle(`${room.name}: AllMyStuff room`);
  });

  // No longer in the room (left via the panel, removed by the host, the
  // room closed, the bar's leave-ask) — the window goes with the call.
  $effect(() => {
    if (joined && !app.isJoined(target)) void hangUp();
  });
</script>

<div class="host">
  {#if app.roomOpenId === target}
    <RoomPanel windowed={true} />
  {:else if !joined}
    <div class="notice">
      <div class="glyph">🪩</div>
      {#if room}
        <p>Joining <b>{room.name}</b>: connecting to the mesh…</p>
      {:else}
        <p>Waiting for this room to land: if you were just invited, its
          details are still on their way over the mesh…</p>
      {/if}
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
