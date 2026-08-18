<script lang="ts">
  // The Rooms tab of the bottom-left sidebar — the zoom-like calls between
  // your machines (and the people you share with). Lists every room this
  // device is entitled to (made here, or invited into — past invites stay
  // listed like roster slots until the host removes this device or closes
  // the room), makes new ones (open or invite-only), and pastes an id to
  // ask in. Joining opens the call: its own OS window on the desktop, the
  // in-page panel (RoomPanel) in the web preview. (The outer panel shell +
  // tab strip live in Sidebar.svelte.)
  import { app } from "../store.svelte";
  import type { RoomAccess } from "../types";

  let draftName = $state("");
  let draftMembers = $state<string[]>([]);
  let draftAccess = $state<RoomAccess>("invite");

  // The "paste an id you were given" affordance.
  let joinOpen = $state(false);
  let joinCode = $state("");

  function toggleMember(id: string) {
    draftMembers = draftMembers.includes(id)
      ? draftMembers.filter((m) => m !== id)
      : [...draftMembers, id];
  }

  function create() {
    app.createRoom(draftName, draftMembers, draftAccess);
    draftName = "";
    draftMembers = [];
    draftAccess = "invite";
  }

  function openDraft() {
    // Pre-fill the default so the maker sees what they'll get: a room
    // named after the fleet's owner ("Casey's room"). Still editable.
    draftName = app.defaultRoomName();
    draftMembers = [];
    draftAccess = "invite";
    joinOpen = false;
    app.roomDraftOpen = true;
  }

  async function knock() {
    if (await app.knockRoom(joinCode)) {
      joinCode = "";
      joinOpen = false;
    }
  }

  function memberSummary(ids: string[]): string {
    const names = [
      ...new Set(ids.filter((id) => !app.isMe(id)).map((id) => app.roomWho(id).who)),
    ].filter(Boolean);
    const shown = names.slice(0, 3).join(", ");
    return names.length > 3 ? `${shown}, +${names.length - 3}` : shown || "just you";
  }

  function presentCount(roomId: string): number {
    return (app.roomPresence[roomId] ?? []).filter((m) => !app.isMe(m)).length;
  }

  // ---- per-room settings menu -----------------------------------------
  //
  // Each room's actions live behind one ⋯ button now (clearer than a bare
  // ✕, and room to grow): Copy Join ID, leave the call, and a red leave /
  // close. The menu is fixed-positioned off the button so the rooms list's
  // own scroll never clips it.
  let menuFor = $state<string | null>(null);
  let menuPos = $state<{ right: number; bottom: number } | null>(null);

  function openMenu(e: MouseEvent, roomId: string) {
    e.stopPropagation();
    if (menuFor === roomId) {
      menuFor = null;
      return;
    }
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    // Anchor the menu's bottom-right just above the button (the bar lives at
    // the screen's bottom-left, so it opens up-and-left).
    menuPos = {
      right: Math.max(8, window.innerWidth - rect.right),
      bottom: Math.max(8, window.innerHeight - rect.top + 6),
    };
    menuFor = roomId;
  }

  // The room whose join id was just copied — its menu item flashes "Copied ✓"
  // for a beat before the menu closes, instead of firing a toast.
  let copiedId = $state<string | null>(null);
  async function copyId(roomId: string) {
    if (await app.copyRoomId(roomId)) {
      copiedId = roomId;
      setTimeout(() => {
        if (copiedId === roomId) copiedId = null;
        if (menuFor === roomId) menuFor = null;
      }, 1100);
    } else {
      menuFor = null;
    }
  }

  // Close the menu on any outside pointer-down (the button + the menu itself
  // are exempt so they can toggle / be clicked).
  $effect(() => {
    function onDown(e: PointerEvent) {
      const t = e.target as Element | null;
      if (!t?.closest?.(".r-menu, .r-gear")) menuFor = null;
    }
    window.addEventListener("pointerdown", onDown);
    return () => window.removeEventListener("pointerdown", onDown);
  });
</script>

<div class="tab-actions">
  {#if app.roomDraftOpen}
    <button class="btn small" onclick={() => (app.roomDraftOpen = false)}>Close</button>
  {:else}
    <button
      class="btn small ghost"
      class:lit={joinOpen}
      title="Paste a room id someone gave you and ask to join"
      onclick={() => (joinOpen = !joinOpen)}>⌁ Join with an id</button
    >
    <button class="btn small primary" onclick={openDraft}>+ New room</button>
  {/if}
</div>

{#if joinOpen && !app.roomDraftOpen}
  <div class="join-code">
    <input
      placeholder="room:…  (paste an id you were given)"
      bind:value={joinCode}
      onkeydown={(e) => e.key === "Enter" && knock()}
    />
    <button class="btn small primary" disabled={!joinCode.trim()} onclick={knock}>Ask in</button>
    <p class="hint faint">
      An <b>open</b> room lets you straight in; an invite-only host gets asked and can admit
      you.
    </p>
  </div>
{/if}

{#if app.roomDraftOpen}
  <!-- Making a room: name it, choose how it admits, pick who's in it. -->
  <div class="draft">
    <p class="hint">
      A room is a call between machines, and <b>you host the rooms you make</b>: their
      identity and roster are this device's. Start one with just this node and invite
      machines later.
    </p>
    <input
      class="name-input"
      placeholder={app.defaultRoomName()}
      bind:value={draftName}
      onkeydown={(e) => e.key === "Enter" && create()}
    />
    <div class="access-pick" role="radiogroup" aria-label="Who can join">
      <button
        class="access-opt"
        class:sel={draftAccess === "invite"}
        role="radio"
        aria-checked={draftAccess === "invite"}
        onclick={() => (draftAccess = "invite")}
      >
        <b>🔒 Invite</b>
        <span>only machines you invite; a pasted id knocks and you admit it</span>
      </button>
      <button
        class="access-opt"
        class:sel={draftAccess === "open"}
        role="radio"
        aria-checked={draftAccess === "open"}
        onclick={() => (draftAccess = "open")}
      >
        <b>🔓 Open</b>
        <span>anyone on a shared network with the room's id walks right in</span>
      </button>
    </div>
    <div class="cands">
      {#each app.roomCandidateNodes as n (n.id)}
        {@const w = app.roomWho(n.id)}
        <label class="cand">
          <input
            type="checkbox"
            checked={draftMembers.includes(n.id)}
            onchange={() => toggleMember(n.id)}
          />
          <span class="cand-name">{w.who}{#if w.machine}&nbsp;<span class="cand-machine">· {w.machine}</span>{/if}</span>
          <span class="dot" class:on={n.online}></span>
        </label>
      {/each}
      {#if app.roomCandidateNodes.length === 0}
        <p class="hint faint">No other machines on the graph yet.</p>
      {/if}
    </div>
    <button class="btn primary small wide" onclick={create}>
      {draftMembers.length === 0 ? "Create room (just this node)" : "Create room"}
    </button>
  </div>
{:else if app.rooms.length === 0}
  <p class="hint">
    No rooms yet. Make one to share your screen, sound or files with several machines at once.
  </p>
{/if}

{#if app.rooms.length > 0 && !app.roomDraftOpen}
  <ul class="rooms">
    {#each app.rooms as r (r.id)}
      <!-- A room being read in its own window owns its unread badge —
           this bar would otherwise count lines the user is looking at. -->
      {@const unread = app.isJoinedAnywhere(r.id) && !app.isJoined(r.id) ? 0 : app.roomUnread[r.id] ?? 0}
      {@const present = presentCount(r.id)}
      {@const knocks = (app.roomKnocks[r.id] ?? []).length}
      {@const joined = app.isJoinedAnywhere(r.id)}
      <li class:open={app.roomOpenId === r.id || (joined && !app.isJoined(r.id))}>
        <div class="r-main">
          <div class="r-name">
            🪩 {r.name}
            {#if app.roomAccess(r) === "open"}<span class="r-open" title="Open room: anyone with its id can join">🔓</span>{/if}
            {#if joined}<span class="in-dot" title="You're in this room"></span>{/if}
            {#if unread > 0}<span class="unread" title="Unread chat">{unread}</span>{/if}
            {#if knocks > 0}<span class="knock-badge" title="{knocks} asking to join: open the room to admit them">{knocks} asking</span>{/if}
          </div>
          <div class="r-sub">
            {app.isRoomHost(r) ? "hosted by you" : `hosted by ${app.roomHostLabel(r)}`}
            · {memberSummary(r.members)}{present > 0 ? ` · ${present} in now` : ""}
          </div>
        </div>
        {#if joined && app.roomOpenId !== r.id}
          <button class="btn small primary" onclick={() => app.joinRoom(r.id)}>Open</button>
        {:else if !joined}
          <button class="btn small primary" onclick={() => app.joinRoom(r.id)}>Join</button>
        {/if}
        <button
          class="btn small ghost r-gear"
          title="Room settings"
          aria-label="Room settings"
          aria-haspopup="menu"
          aria-expanded={menuFor === r.id}
          onclick={(e) => openMenu(e, r.id)}>⋯</button
        >
        {#if menuFor === r.id && menuPos}
          <div
            class="r-menu"
            role="menu"
            style="right: {menuPos.right}px; bottom: {menuPos.bottom}px;"
          >
            <button class="r-menu-item" class:copied={copiedId === r.id} role="menuitem" onclick={() => copyId(r.id)}>
              <span class="rm-icon" aria-hidden="true">{copiedId === r.id ? "✓" : "📋"}</span>
              <span class="rm-text">
                <span class="rm-label">{copiedId === r.id ? "Copied" : "Copy Join ID"}</span>
                <span class="rm-sub">{copiedId === r.id ? "join id on your clipboard" : "share it so a machine can ask in"}</span>
              </span>
            </button>
            {#if joined}
              <button
                class="r-menu-item"
                role="menuitem"
                onclick={() => {
                  app.leaveRoomEverywhere(r.id);
                  menuFor = null;
                }}
              >
                <span class="rm-icon" aria-hidden="true">⏏</span>
                <span class="rm-text">
                  <span class="rm-label">Leave call</span>
                  <span class="rm-sub">hang up: the room stays listed</span>
                </span>
              </button>
            {/if}
            <div class="r-menu-sep"></div>
            <button
              class="r-menu-item danger"
              role="menuitem"
              onclick={() => {
                app.deleteRoom(r.id);
                menuFor = null;
              }}
            >
              <span class="rm-icon" aria-hidden="true">🚪</span>
              <span class="rm-text">
                <span class="rm-label">{app.isRoomHost(r) ? "Close room" : "Leave room"}</span>
                <span class="rm-sub"
                  >{app.isRoomHost(r) ? "ends it for everyone" : "removes it from this device"}</span
                >
              </span>
            </button>
          </div>
        {/if}
      </li>
    {/each}
  </ul>
{/if}

<style>
  .tab-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.3rem;
    margin-bottom: 0.5rem;
  }
  .tab-actions .lit {
    border-color: var(--accent);
    color: var(--accent-ink);
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0 0 0.5rem;
    line-height: 1.4;
  }
  .hint.faint {
    color: var(--ink-faint);
  }
  .join-code {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-bottom: 0.6rem;
  }
  .join-code input {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.35rem 0.5rem;
    font-size: 0.76rem;
    font-family: var(--mono);
    background: var(--surface);
    color: var(--ink);
  }
  .join-code .hint {
    flex-basis: 100%;
    margin: 0;
  }
  .name-input {
    width: 100%;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.5rem;
    font-size: 0.82rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
    margin-bottom: 0.5rem;
  }
  .access-pick {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.4rem;
    margin-bottom: 0.55rem;
  }
  .access-opt {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    text-align: left;
    border: 1px solid var(--line);
    background: var(--surface-2);
    color: var(--ink);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.5rem;
    font-size: 0.74rem;
  }
  .access-opt span {
    color: var(--ink-faint);
    font-size: 0.66rem;
    line-height: 1.35;
  }
  .access-opt.sel {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .cands {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    max-height: 9rem;
    overflow-y: auto;
    margin-bottom: 0.6rem;
  }
  .cand {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    font-size: 0.8rem;
    padding: 0.25rem 0.3rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .cand:hover {
    background: var(--surface-2);
  }
  .cand-name {
    flex: 1;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .cand-machine {
    color: var(--ink-faint);
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .dot.on {
    background: var(--ok);
  }
  .wide {
    width: 100%;
  }
  .rooms {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    max-height: 13rem;
    overflow-y: auto;
  }
  .rooms li {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.35rem 0.4rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.45rem 0.4rem 0.45rem 0.55rem;
  }
  .rooms li.open {
    background: var(--accent-soft);
    box-shadow: 0 0 0 1.5px var(--accent);
  }
  .r-main {
    flex: 1 1 8rem;
    min-width: 0;
  }
  .r-name {
    font-size: 0.85rem;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .r-open {
    font-size: 0.72rem;
  }
  .in-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--ok);
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.16);
  }
  .unread {
    background: var(--accent);
    color: #fff;
    font-size: 0.62rem;
    font-weight: 700;
    border-radius: var(--r-pill);
    padding: 0.05rem 0.36rem;
    line-height: 1.2;
  }
  .knock-badge {
    background: var(--warn-soft);
    color: var(--warn);
    border: 1px solid oklch(0.79 0.14 75 / 0.4);
    font-size: 0.6rem;
    font-weight: 700;
    border-radius: var(--r-pill);
    padding: 0.03rem 0.36rem;
    line-height: 1.3;
    white-space: nowrap;
  }
  .r-sub {
    font-size: 0.72rem;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .r-gear {
    flex-shrink: 0;
    padding: 0.25rem 0.45rem;
    color: var(--ink-faint);
    font-weight: 700;
  }
  .r-gear:hover {
    color: var(--ink);
  }
  .r-menu {
    position: fixed;
    z-index: 70;
    min-width: 13rem;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-lg);
    padding: 0.35rem;
    animation: rmenu 0.12s ease;
  }
  @keyframes rmenu {
    from {
      transform: translateY(4px);
      opacity: 0;
    }
  }
  .r-menu-item {
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
    width: 100%;
    border: none;
    background: transparent;
    color: var(--ink);
    text-align: left;
    padding: 0.4rem 0.5rem;
    border-radius: var(--r-sm);
    cursor: pointer;
    font: inherit;
  }
  .r-menu-item:hover {
    background: var(--surface-2);
  }
  /* The transient "Copied ✓" state after copying the join id. */
  .r-menu-item.copied,
  .r-menu-item.copied:hover {
    background: color-mix(in oklab, var(--ok) 14%, transparent);
  }
  .r-menu-item.copied .rm-icon,
  .r-menu-item.copied .rm-label {
    color: var(--ok);
  }
  .rm-icon {
    font-size: 0.95rem;
    line-height: 1.25;
    flex-shrink: 0;
  }
  .rm-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .rm-label {
    font-size: 0.82rem;
    font-weight: 600;
  }
  .rm-sub {
    font-size: 0.68rem;
    color: var(--ink-faint);
    line-height: 1.3;
  }
  .r-menu-sep {
    height: 1px;
    background: var(--line);
    margin: 0.3rem 0.2rem;
  }
  .r-menu-item.danger .rm-label {
    color: var(--danger);
  }
  .r-menu-item.danger:hover {
    background: var(--danger-soft);
  }
</style>
