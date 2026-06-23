<script lang="ts">
  // The fleet pill's dropdown — the sibling of the meshes and venues menus. A
  // fleet is a closed mesh with a custom label, so it earns the same shape: the
  // fleets you're in, each with its mesh, an inline rename for the ones you own,
  // a roster preview, and a unified Leave (leaving the fleet leaves its mesh —
  // they're the same act). The backend tracks one fleet today; this loops over
  // `app.fleets` so the multi-fleet expansion is a data change, not a rewrite.
  import { app } from "../store.svelte";
  import type { OwnedRoster } from "../types";

  function close() {
    app.fleetMenuOpen = false;
  }

  // Close on a click anywhere outside the menu (the pill itself stops
  // propagation so it can toggle).
  function onWindowPointerDown(e: PointerEvent) {
    const t = e.target as Element | null;
    if (!t?.closest?.(".fleet-menu, .chip.fleet")) close();
  }

  $effect(() => {
    window.addEventListener("pointerdown", onWindowPointerDown);
    return () => window.removeEventListener("pointerdown", onWindowPointerDown);
  });

  // A stable key per fleet for the name drafts (the closed-network id, falling
  // back to the shared key for a not-yet-founded fleet).
  function fleetId(f: OwnedRoster): string {
    return f.network_id?.trim() || f.key || "fleet";
  }

  // The fleet's display name: its explicit name, else "<owner>'s fleet" from the
  // roster, else a neutral label.
  function fleetLabel(f: OwnedRoster): string {
    const name = f.name?.trim();
    if (name) return name;
    const owner = f.members.find((m) => m.role === "owner");
    const on = owner?.label?.trim();
    return on ? `${on}'s fleet` : "Your fleet";
  }

  // This device's role in a given fleet, in the clarified terms (a "manager" is
  // a MyOwnMesh "controller").
  function selfRoleIn(f: OwnedRoster): "owner" | "manager" | "member" | null {
    const m = f.members.find((x) => app.isMe(x.device));
    if (!m) return null;
    if (m.role === "owner") return "owner";
    if (m.role === "controller") return "manager";
    return "member";
  }

  function roleLabel(role: OwnedRoster["members"][number]["role"]): string {
    if (role === "owner") return "owner";
    if (role === "controller") return "manager";
    return "member";
  }

  // The live mesh backing a fleet, if it's joined (the closed network it rides
  // on). This is what "move the fleet meshes into the fleet pill menu" means.
  function meshFor(f: OwnedRoster) {
    const id = f.network_id?.trim();
    if (!id) return null;
    return app.fleetMeshes.find((n) => n.network_id === id) ?? null;
  }

  // ---- inline rename (owner-only), one draft per fleet ----
  let drafts = $state<Record<string, string>>({});
  let dirty = $state<Record<string, boolean>>({});
  $effect(() => {
    // Re-seed each fleet's draft from the live roster whenever it converges,
    // unless the user is mid-edit on that one.
    for (const f of app.fleets) {
      const id = fleetId(f);
      if (!dirty[id]) drafts[id] = f.name?.trim() ?? "";
    }
  });
  function saveName(f: OwnedRoster) {
    const id = fleetId(f);
    dirty[id] = false;
    const next = (drafts[id] ?? "").trim();
    if (next === (f.name?.trim() ?? "")) return;
    // The backend renames the fleet this device belongs to. (A per-fleet id
    // rides in with the multi-fleet backend.)
    void app.setFleetName(next);
  }

  // ---- two-step Leave confirm (armed id = fleet id) ----
  let armed = $state<string | null>(null);
  function confirmThen(id: string, act: () => void) {
    if (armed === id) {
      armed = null;
      act();
    } else {
      armed = id;
      setTimeout(() => {
        if (armed === id) armed = null;
      }, 3500);
    }
  }
</script>

<div class="fleet-menu" role="menu" aria-label="Your fleets">
  <div class="menu-head">Your fleets</div>

  {#if !app.inFleet}
    <p class="menu-empty">
      You're not in a fleet yet. Claim a device that's offering itself — open it
      from the graph and choose <b>Claim this device</b> — and it joins a fresh
      fleet with this one. More in
      <button class="linkish" onclick={() => (close(), app.openSettings("fleet"))}>Fleet settings</button>.
    </p>
  {/if}

  {#each app.fleets as f (fleetId(f))}
    {@const id = fleetId(f)}
    {@const owner = f.is_owner ?? false}
    {@const role = selfRoleIn(f)}
    {@const mesh = meshFor(f)}
    <section class="fleet">
      <!-- Name: inline-editable for an owner, read-only otherwise — one of the
           many easy places an owner can set a fleet's name. -->
      <div class="fleet-head">
        {#if owner}
          <input
            class="name-input"
            placeholder="Name this fleet…"
            aria-label="Fleet name"
            bind:value={drafts[id]}
            oninput={() => (dirty[id] = true)}
            onkeydown={(e) => e.key === "Enter" && saveName(f)}
            onblur={() => saveName(f)}
          />
        {:else}
          <div class="fleet-name" title="Only the fleet owner can rename it">{fleetLabel(f)}</div>
        {/if}
        {#if role}<span class="role-tag" class:owner={role === "owner"} class:manager={role === "manager"}>you · {role}</span>{/if}
      </div>

      <!-- The fleet's mesh — the closed network it rides on. It lives here, not
           in the meshes menu: you join and leave it by joining and leaving the
           fleet. -->
      {#if mesh}
        <div class="row mesh">
          <span class="row-dot live"></span>
          <div class="row-main">
            <div class="row-name">{app.meshLabel(mesh)}<span class="mesh-tag">🔗 fleet mesh</span></div>
            <div class="row-sub">{mesh.network_id}</div>
          </div>
          <span class="lock" title="The fleet's closed mesh — leave the fleet to leave it." aria-label="Fleet mesh">🔒</span>
        </div>
      {/if}

      <!-- Members + their roles (member: can be controlled · manager: can
           control · owner: controls and sets managers/owners). -->
      <div class="members">
        {#each f.members as m (m.device)}
          {@const isSelf = app.isMe(m.device)}
          <div class="row">
            <span class="row-dot owner-dot" class:owner={m.role === "owner"}></span>
            <div class="row-main">
              <div class="row-name">{m.label || m.device.slice(0, 12)}{#if isSelf} <span class="self-tag">this device</span>{/if}</div>
            </div>
            <span class="role-pill" class:owner={m.role === "owner"} class:manager={m.role === "controller"}>{roleLabel(m.role)}</span>
          </div>
        {/each}
      </div>

      <div class="fleet-foot">
        <button
          class="btn small leave"
          class:armed={armed === id}
          title="Leave the fleet — and its mesh. The shared key drops here and this device goes back to unclaimed."
          onclick={() => confirmThen(id, () => (close(), void app.leaveFleet()))}
        >
          {armed === id ? "Leave the fleet — sure?" : "Leave the fleet"}
        </button>
        <button class="btn small" onclick={() => (close(), app.openSettings("fleet"))}>⚙ Fleet settings…</button>
      </div>
    </section>
  {/each}
</div>

<style>
  .fleet-menu {
    position: absolute;
    top: calc(100% + 0.45rem);
    right: 0;
    width: 18.5rem;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-lg);
    padding: 0.45rem;
    z-index: 60;
    animation: drop 0.12s ease;
    text-align: left;
  }
  @keyframes drop {
    from {
      transform: translateY(-4px);
      opacity: 0;
    }
  }
  .menu-head {
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--ink-faint);
    padding: 0.25rem 0.45rem 0.4rem;
  }
  .menu-empty {
    font-size: 0.78rem;
    color: var(--ink-soft);
    line-height: 1.45;
    margin: 0 0 0.3rem;
    padding: 0 0.45rem;
  }
  .linkish {
    border: none;
    background: none;
    color: var(--accent-ink);
    padding: 0;
    font-size: inherit;
    text-decoration: underline;
    cursor: pointer;
  }
  .fleet + .fleet {
    border-top: 1px solid var(--line);
    margin-top: 0.4rem;
    padding-top: 0.4rem;
  }
  .fleet-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.1rem 0.2rem 0.4rem;
  }
  .name-input {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.3rem 0.5rem;
    font-size: 0.84rem;
    font-weight: 650;
    font-family: inherit;
    background: var(--surface-2);
    color: var(--ink);
  }
  .name-input:focus {
    outline: none;
    border-color: var(--accent);
    background: var(--surface);
  }
  .fleet-name {
    flex: 1;
    min-width: 0;
    font-size: 0.86rem;
    font-weight: 700;
    color: var(--accent-ink);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .role-tag {
    flex-shrink: 0;
    font-size: 0.6rem;
    font-weight: 700;
    color: var(--ink-soft);
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  .role-tag.owner,
  .role-tag.manager {
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-color: var(--accent-soft);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.35rem 0.45rem;
    border-radius: var(--r-sm);
  }
  .row:hover {
    background: var(--surface-2);
  }
  .row.mesh {
    box-shadow: inset 0 0 0 1px var(--accent-soft);
  }
  .row-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .row-dot.live {
    background: var(--ok);
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.16);
  }
  .row-dot.owner-dot {
    background: var(--ink-faint);
  }
  .row-dot.owner-dot.owner {
    background: var(--accent);
  }
  .row-main {
    flex: 1;
    min-width: 0;
  }
  .row-name {
    font-size: 0.82rem;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }
  .row-sub {
    font-size: 0.66rem;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .self-tag {
    font-size: 0.58rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.35rem;
  }
  .mesh-tag {
    margin-left: 0.35rem;
    font-size: 0.6rem;
    font-weight: 700;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.35rem;
  }
  .role-pill {
    flex-shrink: 0;
    font-size: 0.6rem;
    font-weight: 700;
    color: var(--ink-faint);
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  .role-pill.owner,
  .role-pill.manager {
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-color: var(--accent-soft);
  }
  .lock {
    flex-shrink: 0;
    font-size: 0.9rem;
    opacity: 0.75;
    cursor: not-allowed;
    padding: 0 0.2rem;
  }
  .members {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .fleet-foot {
    display: flex;
    gap: 0.4rem;
    margin-top: 0.45rem;
    padding-top: 0.4rem;
    border-top: 1px solid var(--line);
  }
  .fleet-foot .btn {
    flex: 1;
    justify-content: center;
  }
  .leave.armed {
    border-color: oklch(0.7 0.19 14 / 0.5);
    color: var(--danger);
    background: var(--danger-soft);
  }
</style>
