<script lang="ts">
  // Fleet pane: the "Owned" roster: the devices you've claimed, linked by a
  // single shared key gossiped between them. For now this only groups your
  // machines internally; a later edition lets you hand that key to other
  // things.
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { displayName } from "../../types";
  import {
    fleetMfaStatus,
    fleetMfaEnroll,
    fleetMfaDisable,
    type FleetMfaEnrolled,
  } from "../../tauri";

  const fleet = $derived(app.ownedFleet);
  const members = $derived(fleet?.members ?? []);
  // One membership truth, shared with the graph and the drawer: the backend's
  // `in_fleet`. So the settings pane can't say "no fleet" while the drawer says
  // you're in one. A keyless member (claimed, awaiting its key) is in a fleet
  // too: it just has no key block to show.
  const hasFleet = $derived(app.inFleet);
  const hasKey = $derived(!!fleet?.key);
  // Membership is the permission: you can leave, and kick others, while this
  // device is in the fleet: the same single flag.
  const selfIsMember = $derived(app.inFleet);

  let revealed = $state(false);
  let copied = $state(false);
  // The fleet-name editor. Seeded from the roster whenever it converges
  // (a rename gossiped from another member lands here too), unless the
  // user is mid-edit.
  let nameDraft = $state("");
  let nameDirty = $state(false);
  $effect(() => {
    const live = app.fleetName;
    if (!nameDirty) nameDraft = live;
  });

  function saveName() {
    nameDirty = false;
    if (nameDraft.trim() === app.fleetName) return;
    void app.setFleetName(nameDraft);
  }

  // ---- infra hubs (governed topology) --------------------------------
  //
  // The owner stages hub toggles on the member rows, then signs the whole
  // shape in one governance transition (`fleet_set_hubs`). Every member's
  // daemon (≥ 0.2.36) converges onto it via the signed log: nothing to
  // configure per device. Draft state mirrors the fleet-name editor:
  // seeded from the converged roster unless the user is mid-edit.

  /** Canonical pubkey half of a device id (mirror of the backend's
   *  `pubkey_part`: base32 has no dash, so everything past the first
   *  dash is display suffix). */
  const canon = (id: string) => id.split("-")[0];

  const governedTopo = $derived(fleet?.topology ?? null);
  const governedHubs = $derived(
    governedTopo?.kind === "hubs" ? governedTopo.hubs : [],
  );
  const governedRedundancy = $derived(
    governedTopo?.kind === "hubs" ? governedTopo.spoke_redundancy : null,
  );

  let hubsDraft = $state<string[]>([]);
  let redundancyDraft = $state<number | null>(null);
  let hubsDirty = $state(false);
  $effect(() => {
    const live = governedHubs;
    const liveR = governedRedundancy;
    if (!hubsDirty) {
      hubsDraft = [...live];
      redundancyDraft = liveR;
    }
  });

  const isHubDraft = (device: string) =>
    hubsDraft.some((h) => canon(h) === canon(device));
  const isHubLive = (device: string) =>
    governedHubs.some((h) => canon(h) === canon(device));

  function toggleHub(device: string) {
    hubsDirty = true;
    const c = canon(device);
    hubsDraft = hubsDraft.some((h) => canon(h) === c)
      ? hubsDraft.filter((h) => canon(h) !== c)
      : [...hubsDraft, c];
  }

  async function applyHubs() {
    await app.setFleetHubs(hubsDraft, redundancyDraft ?? undefined);
    // Converged roster (or the failure toast) is the outcome; either way
    // the draft re-seeds from live state.
    hubsDirty = false;
  }

  function cancelHubs() {
    hubsDirty = false;
    hubsDraft = [...governedHubs];
    redundancyDraft = governedRedundancy;
  }

  // Remote claiming by code: the WAN path behind the public-claims toggle.
  // Slow by nature (joins a randomized rendezvous network and waits for the
  // device to appear), so it carries its own busy + error state.
  let claimCode = $state("");
  let claiming = $state(false);
  let claimErr = $state<string | null>(null);
  let claimIdCopied = $state(false);
  const localClaimId = $derived(fleet?.claim_code?.trim() ?? "");

  async function copyLocalClaimId() {
    if (!localClaimId) return;
    try {
      await navigator.clipboard.writeText(localClaimId);
      claimIdCopied = true;
      setTimeout(() => (claimIdCopied = false), 1500);
    } catch {
      app.toast("warn", "Couldn't copy the remote claim ID");
    }
  }

  async function claimByCode() {
    const code = claimCode.trim();
    if (!code || claiming) return;
    claiming = true;
    claimErr = null;
    try {
      await app.claimRemoteByCode(code);
      claimCode = "";
    } catch (e) {
      claimErr = String(e);
    } finally {
      claiming = false;
    }
  }
  // Two-step confirm: first click arms (shows "sure?"), second acts. The
  // armed id is the member's device (or "leave" for the leave button).
  let armed = $state<string | null>(null);

  onMount(() => {
    void app.loadOwnedFleet();
    void loadMfaStatus();
  });

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

  // Show the key as a short, safe-to-glance fingerprint unless revealed.
  const keyShown = $derived.by(() => {
    const k = fleet?.key ?? "";
    if (!k) return "";
    if (revealed) return k;
    return `${k.slice(0, 6)}…${k.slice(-4)}`;
  });

  async function copyKey() {
    if (!fleet?.key) return;
    try {
      await navigator.clipboard.writeText(fleet.key);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      app.toast("warn", "Couldn't copy the key");
    }
  }

  // Resolve a roster device id to its display name the same way the graph and
  // drawer do: by *canonical* machine match, not a strict id equality. A
  // roster id can be a different form of the same machine's id (bare pubkey vs
  // display id), so the strict lookup missed it and the name vanished.
  function nodeLabel(device: string): string {
    const n = app.machineByAnyId(device);
    return n ? displayName(n) : "";
  }

  function memberName(m: { device: string; label: string }): string {
    // This device always knows its own name, even when the roster didn't stamp
    // a label on it. Otherwise prefer the live node's name, then the roster
    // label, then a short id as a last resort.
    if (app.isMe(m.device)) {
      return (app.localNode ? displayName(app.localNode) : "") || nodeLabel(m.device) || m.label || "This device";
    }
    return nodeLabel(m.device) || m.label || m.device.slice(0, 12);
  }

  // "View on the graph": the explicit way out of Settings to a device. Clicking
  // a row no longer yanks you to the graph; this dedicated button does. It
  // resolves the roster id to the live node first (a roster id is often a
  // different *form* of the same machine's id than the graph lays out under, so
  // selecting the raw id highlighted nothing), then selects and centres it.
  function viewOnGraph(device: string) {
    app.focusNode(device);
  }

  // Roles are layered: promote stages a member up to Manager, then a Manager to
  // co-owner, with a matching step-down. The mesh layer (MyOwnMesh's closed-
  // network governance) is the authority, so the buttons are gated to exactly
  // what it enforces: only an owner can grant or withdraw a manager or owner,
  // and an owner or manager can evict a member: and we just float the signed
  // proposal for the daemon to ratify.
  function promoteToManager(device: string) {
    void app.grantFleetRole(device, "manager");
  }
  function promoteToOwner(device: string) {
    void app.grantFleetRole(device, "owner");
  }
  // An owner steps back to manager by re-granting the lower role; a manager
  // steps back to a plain member by a revoke (which drops to Member).
  function demoteToManager(device: string) {
    void app.grantFleetRole(device, "manager");
  }
  function demoteToMember(device: string) {
    void app.withdrawFleetRole(device);
  }

  // ---- fleet custody (TOTP / MFA) ----
  // The fleet is a closed network underneath; enrolling an authenticator on
  // this device tells the daemon to refuse a fleet governance change (kind
  // flip, owner grant/revoke) without a fresh code. It guards *this device's*
  // signing key for the fleet: it doesn't replace the shared fleet key.
  let mfaEnrolled = $state(false);
  let mfaBusy = $state(false);
  let mfaError = $state<string | null>(null);
  let mfaEnrollResult = $state<FleetMfaEnrolled | null>(null);
  let mfaDisableCode = $state("");
  let mfaDisableOpen = $state(false);

  async function loadMfaStatus() {
    try {
      const s = await fleetMfaStatus();
      mfaEnrolled = s.enrolled;
    } catch {
      // status is best-effort; a missing fleet just reads as not-enrolled.
      mfaEnrolled = false;
    }
  }

  async function enrollMfa() {
    mfaBusy = true;
    mfaError = null;
    try {
      mfaEnrollResult = await fleetMfaEnroll();
      mfaEnrolled = true;
    } catch (e) {
      mfaError = e instanceof Error ? e.message : String(e);
    }
    mfaBusy = false;
  }

  async function disableMfa() {
    const code = mfaDisableCode.trim();
    if (!code) return;
    mfaBusy = true;
    mfaError = null;
    try {
      await fleetMfaDisable(code);
      mfaEnrolled = false;
      mfaEnrollResult = null;
      mfaDisableCode = "";
      mfaDisableOpen = false;
    } catch (e) {
      mfaError = e instanceof Error ? e.message : String(e);
    }
    mfaBusy = false;
  }
</script>

<div class="section">
  <h3>Fleet</h3>
  <p class="lead">
    Your claimed devices and their fleet roles.
  </p>

  {#snippet claimingBlock()}
    <!-- Sits under the device list, per the claiming model: LAN-first,
         public-mesh only by deliberate, device-local choice. -->
    <section class="block">
      <h4>🌐 Claiming over the public mesh</h4>
      <label class="pc-row">
        <input
          type="checkbox"
          checked={app.publicClaims}
          onchange={(e) => void app.setPublicClaims(e.currentTarget.checked)}
        />
        <span>Allow claiming over the public mesh</span>
      </label>
      <p class="hint">
        This device only. Leave this off for seamless LAN claiming. Turn it on
        when either device is remote.
      </p>
      {#if app.publicClaims}
        <div class="claim-direction">
          <div class="claim-direction-title">This device's remote claim ID</div>
          {#if localClaimId}
            <div class="local-claim-id-row">
              <code>{localClaimId}</code>
              <button
                class="btn small"
                aria-label="Copy this device's remote claim ID"
                onclick={() => void copyLocalClaimId()}
              >
                {claimIdCopied ? "Copied" : "Copy"}
              </button>
            </div>
            <p class="hint">
              Enter this ID on the computer that will claim this device.
            </p>
          {:else}
            <p class="hint claim-unavailable">
              Choose <b>Make claimable</b> on this device's graph card to create
              its remote claim ID. A device already in a fleet cannot be claimed.
            </p>
          {/if}
        </div>

        <div class="claim-direction">
          <div class="claim-direction-title">Claim a remote device</div>
          <div class="claim-code-row">
            <input
              class="mfa-input code-input"
              aria-label="Remote claim ID"
              placeholder="Remote claim ID (xxxx-xxxx-…)"
              bind:value={claimCode}
              disabled={claiming}
              onkeydown={(e) => {
                if (e.key === "Enter") void claimByCode();
              }}
            />
            <button
              class="btn small primary"
              disabled={claiming || !claimCode.trim()}
              onclick={() => void claimByCode()}
            >
              {claiming ? "Claiming…" : "Claim device"}
            </button>
          </div>
          {#if claimErr}
            <div class="mfa-status err" role="alert">⚠ {claimErr}</div>
          {/if}
          <p class="hint">
            Enter the remote claim ID shown on the other device.
          </p>
        </div>
      {/if}
    </section>
  {/snippet}

  {#if hasFleet}
    <!-- Whose fleet this is: the owning *person's* name, which leads. It's not
         the fleet's mesh id (that's the word-salad network name over in Meshes),
         and it's not a device name: the owner *machines* are marked ★ Owner in
         the device list below (they're fleet owners too, just identified by
         their device name). -->
    <section class="block name-block">
      <div class="name-row">
        <label class="name-label" for="fleet-owner-name">👤 Fleet owner</label>
        {#if app.isFleetOwner}
          <input
            id="fleet-owner-name"
            class="name-input"
            placeholder="The person who owns this fleet…"
            bind:value={nameDraft}
            oninput={() => (nameDirty = true)}
            onkeydown={(e) => e.key === "Enter" && saveName()}
            onblur={saveName}
          />
        {:else}
          <!-- Non-owners can't change it, but they (and everyone in the mesh)
               see it: plain text, not a greyed-out field. -->
          <div class="name-value" class:unnamed={!app.fleetName}>
            {app.fleetName || "Unnamed owner"}
          </div>
        {/if}
      </div>
      <p class="hint">
        The owner's name appears on the graph and in new rooms.
        {#if !app.isFleetOwner} Only the fleet owner can change it.{/if}
      </p>
    </section>

    {#if hasKey}
      <section class="block key-block">
        <div class="key-head">
          <span class="key-title">🔑 Fleet key</span>
          <span class="muted">v{fleet?.version}</span>
        </div>
        <div class="key-row">
          <code class:revealed>{keyShown}</code>
          <button class="btn small" onclick={() => (revealed = !revealed)}>{revealed ? "Hide" : "Reveal"}</button>
          <button class="btn small" onclick={copyKey}>{copied ? "Copied ✓" : "Copy"}</button>
        </div>
        <p class="hint">Every fleet device holds this key. Keep it private.</p>
      </section>
    {:else}
      <section class="block">
        <p class="hint">
          Waiting for the fleet owner to provide the shared key.
        </p>
      </section>
    {/if}

    <section class="block">
      <h4>{members.length} device{members.length === 1 ? "" : "s"} in your fleet</h4>
      <ul class="members">
        {#each members as m (m.device)}
          {@const isSelf = app.isMe(m.device)}
          {@const isOwner = app.fleetRoleOf(m.device) === "owner"}
          {@const isManager = app.fleetRoleOf(m.device) === "manager"}
          {@const memberNode = app.machineByAnyId(m.device)}
          {@const isKvm = app.isKvm(memberNode)}
          <li class:owner={isOwner}>
            <div class="m-id-wrap">
              <span class="m-avatar" aria-hidden="true">{isSelf ? "💻" : "🖥"}</span>
              <div class="m-id">
                <div class="m-name">
                  {memberName(m)}
                  {#if isSelf} <span class="self-tag">this device</span>{/if}
                  {#if isOwner} <span class="owner-tag">★ Owner</span>
                  {:else if isManager} <span class="mgr-tag">Manager</span>{/if}
                  {#if isHubLive(m.device)} <span class="hub-tag">⬢ Hub</span>{/if}
                </div>
                <div class="m-sub" title={m.device}>{m.device.slice(0, 18)}…</div>
              </div>
            </div>
            <div class="m-actions">
              <button
                class="role-btn"
                title="Show this device on the graph: leaves Settings, then selects and centres it"
                onclick={() => viewOnGraph(m.device)}
              >
                🔍 View
              </button>
              {#if app.isFleetOwner}
                <button
                  class="role-btn hub-toggle"
                  class:hub-on={isHubDraft(m.device)}
                  title={isHubDraft(m.device)
                    ? "Staged as an infra hub: sign the shape below to apply"
                    : "Stage this device as an infra hub: hubs full-mesh each other and carry the rest of the fleet, so spokes keep a couple of links instead of one per device. Pick always-on boxes."}
                  onclick={() => toggleHub(m.device)}
                >
                  {isHubDraft(m.device) ? "⬢ Hub ✓" : "⬢ Hub"}
                </button>
              {/if}
              <!-- Controls reflect what the mesh layer actually enforces (these
                   are MyOwnMesh's closed-network roles): only an owner can make
                   or withdraw managers and owners; a manager (controller) can
                   only evict a member; a member changes nothing. Evicting an
                   owner outright needs every owner's consent, so it isn't
                   offered: step an owner down to manager first, then evict. -->
              {#if !isSelf}
                {@const iOwn = app.myFleetRole === "owner"}
                {@const iManage = iOwn || app.myFleetRole === "manager"}
                {#if isKvm}
                  {#if (isManager && iOwn) || (!isOwner && !isManager && iManage)}
                    <button
                      class="kick"
                      class:armed={armed === m.device}
                      title="Evict this KVM from the fleet: KVMs are fixed-role appliances and cannot be promoted or demoted"
                      onclick={() => confirmThen(m.device, () => void app.kickFleetMember(m.device))}
                    >
                      {armed === m.device ? "Evict: sure?" : "Evict"}
                    </button>
                  {/if}
                {:else if isOwner}
                  {#if iOwn}
                    <button
                      class="role-btn down"
                      title="Step this co-owner back down to manager: they keep authority to admit members, but lose owner authority. (Evicting an owner outright needs every owner's consent, so step them down first.)"
                      onclick={() => demoteToManager(m.device)}
                    >
                      ⤓ Make manager
                    </button>
                  {/if}
                {:else if isManager}
                  {#if iOwn}
                    <button
                      class="role-btn up"
                      title="Promote this manager to a co-owner: full fleet authority alongside you. Only an owner can make an owner."
                      onclick={() => promoteToOwner(m.device)}
                    >
                      ★ Make owner
                    </button>
                    <button
                      class="role-btn down"
                      title="Withdraw this manager back to a plain member: they keep fleet membership but lose authority to admit members."
                      onclick={() => demoteToMember(m.device)}
                    >
                      ⤓ Make member
                    </button>
                    <button
                      class="kick"
                      class:armed={armed === m.device}
                      title="Evict this device from the fleet: a signed removal that propagates to every member, so a lost or stolen device loses control everywhere"
                      onclick={() => confirmThen(m.device, () => void app.kickFleetMember(m.device))}
                    >
                      {armed === m.device ? "Evict: sure?" : "Evict"}
                    </button>
                  {/if}
                {:else}
                  {#if iOwn}
                    <button
                      class="role-btn up"
                      title="Promote this member to a manager: they can admit members. Promote again afterwards to make them a co-owner. (Only an owner can promote.)"
                      onclick={() => promoteToManager(m.device)}
                    >
                      ★ Make manager
                    </button>
                  {/if}
                  {#if iManage}
                    <button
                      class="kick"
                      class:armed={armed === m.device}
                      title="Evict this device from the fleet: a signed removal that propagates to every member, so a lost or stolen device loses control everywhere"
                      onclick={() => confirmThen(m.device, () => void app.kickFleetMember(m.device))}
                    >
                      {armed === m.device ? "Evict: sure?" : "Evict"}
                    </button>
                  {/if}
                {/if}
              {/if}
            </div>
          </li>
        {/each}
      </ul>
      {#if app.myFleetRole === "manager"}
        <p class="hint">
          As a manager you can evict a member; promoting and withdrawing roles
          is the owner's call. This device can leave on its own below.
        </p>
      {:else if app.myFleetRole !== "owner"}
        <p class="hint">
          Only an owner can change roles; an owner or a manager can evict a
          member. This device can leave on its own below.
        </p>
      {/if}
    </section>

    <section class="block">
      <h4>⬢ Connection shape</h4>
      {#if governedTopo === null}
        <p class="hint">
          Full mesh: every device connects directly to every other device.
          {#if app.isFleetOwner}
            Mark always-on devices as <b>⬢ Hub</b> to reduce connections in a large fleet.
          {/if}
        </p>
      {:else if governedTopo.kind === "hubs"}
        <p class="hint">
          {governedHubs.length} hub{governedHubs.length === 1 ? "" : "s"}; each
          other device keeps {governedRedundancy ?? 2} hub
          link{(governedRedundancy ?? 2) === 1 ? "" : "s"}.
        </p>
      {:else if governedTopo.kind === "full_mesh"}
        <p class="hint">
          Every device connects directly to every other device.
          {#if app.isFleetOwner} Mark members as <b>⬢ Hub</b> to use a hub tier.{/if}
        </p>
      {:else}
        <p class="hint">Owner-signed shape: {governedTopo.kind}.</p>
      {/if}

      {#if app.isFleetOwner && hubsDirty}
        <div class="hub-apply">
          {#if hubsDraft.length > 0}
            <label class="hub-redundancy">
              links per spoke
              <input
                type="number"
                min="1"
                max={Math.max(hubsDraft.length, 1)}
                placeholder="2"
                value={redundancyDraft ?? ""}
                oninput={(e) => {
                  const v = (e.currentTarget as HTMLInputElement).value;
                  redundancyDraft = v === "" ? null : Math.max(1, Number(v));
                }}
              />
            </label>
          {:else}
            <span class="hint-inline">
              No hubs staged: signing returns the fleet to full mesh.
            </span>
          {/if}
          <button class="btn small primary" onclick={() => void applyHubs()}>
            {hubsDraft.length > 0
              ? `Sign shape (${hubsDraft.length} hub${hubsDraft.length === 1 ? "" : "s"})`
              : "Sign full mesh"}
          </button>
          <button class="btn small" onclick={cancelHubs}>Cancel</button>
        </div>
      {/if}
    </section>

    {@render claimingBlock()}

    {#if selfIsMember}
      <section class="block mfa-block">
        <h4>🛡️ Fleet security · authenticator</h4>
        <p class="hint">
          Require an authenticator code when this device signs fleet changes.
        </p>

        {#if mfaEnrolled}
          <div class="mfa-status on">✓ An authenticator is enrolled on this device for the fleet.</div>

          <div class="mfa-disable">
            <button
              class="btn small"
              onclick={() => (mfaDisableOpen = !mfaDisableOpen)}
            >
              {mfaDisableOpen ? "Cancel" : "Remove authenticator"}
            </button>
            {#if mfaDisableOpen}
              <div class="mfa-disable-row">
                <input
                  class="mfa-input"
                  type="text"
                  inputmode="numeric"
                  autocomplete="one-time-code"
                  placeholder="Current 6-digit or recovery code"
                  bind:value={mfaDisableCode}
                  onkeydown={(e) => e.key === "Enter" && disableMfa()}
                />
                <button
                  class="btn small danger"
                  disabled={mfaBusy || !mfaDisableCode.trim()}
                  onclick={disableMfa}
                >
                  Disable
                </button>
              </div>
            {/if}
          </div>
        {:else}
          <button class="btn small primary" disabled={mfaBusy} onclick={enrollMfa}>
            Enroll an authenticator
          </button>
        {/if}

        {#if mfaEnrollResult}
          <div class="mfa-enroll">
            <p class="mfa-enroll-lead">
              <b>Add this to your authenticator app now, and save the recovery
              codes</b>: they won't be shown again.
            </p>
            <div class="mfa-kv"><span>Secret</span><code>{mfaEnrollResult.secret}</code></div>
            <div class="mfa-kv"><span>otpauth URI</span><code class="wrap">{mfaEnrollResult.otpauth_uri}</code></div>
            <div class="mfa-kv">
              <span>Recovery codes</span>
              <ul class="mfa-recovery">
                {#each mfaEnrollResult.recovery_codes as rc (rc)}
                  <li><code>{rc}</code></li>
                {/each}
              </ul>
            </div>
            <button class="btn small" onclick={() => (mfaEnrollResult = null)}>
              I've saved these
            </button>
          </div>
        {/if}

        {#if mfaError}
          <div class="mfa-status err" role="alert">⚠ {mfaError}</div>
        {/if}
      </section>

      <!-- The exit lives at the very bottom, away from the everyday stuff. -->
      <section class="block">
        <div class="leave-row">
          <button
            class="btn small leave"
            class:armed={armed === "leave"}
            title="Remove this device from the fleet (its owner is released too)"
            onclick={() => confirmThen("leave", () => void app.leaveFleet())}
          >
            {armed === "leave" ? "Leave the fleet: sure?" : "Leave the fleet"}
          </button>
          <span class="hint">
            Leaving removes this device's fleet key and ownership.
          </span>
        </div>
      </section>
    {/if}
  {:else}
    <section class="block empty-block">
      <div class="empty-orb">🔗</div>
      <div class="empty-title">No fleet yet</div>
      <p class="hint center">
        Select a claimable device on the graph and choose <b>Claim this device</b>.
      </p>
    </section>

    <!-- Claiming settings exist before the first claim too: founding a fleet
         by claiming a remote device starts right here. -->
    {@render claimingBlock()}
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
  .lead {
    font-size: 0.84rem;
    color: var(--ink-soft);
    line-height: 1.5;
    margin: 0 0 0.4rem;
  }
  .block {
    border-top: 1px solid var(--line);
    padding: 0.9rem 0;
  }
  h4 {
    margin: 0 0 0.5rem;
    font-size: 0.92rem;
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-faint);
    line-height: 1.45;
    margin: 0.4rem 0 0;
  }
  .hint.center {
    text-align: center;
    max-width: 24rem;
  }
  .muted {
    font-size: 0.74rem;
    color: var(--ink-faint);
  }
  .key-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 0.4rem;
  }
  .key-title {
    font-size: 0.9rem;
    font-weight: 650;
  }
  .key-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .name-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    margin-top: 0.7rem;
  }
  .name-label {
    flex-shrink: 0;
    font-size: 0.9rem;
    font-weight: 650;
  }
  .name-input {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.6rem;
    font-size: 0.86rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  .name-input:disabled {
    opacity: 0.6;
  }
  /* The non-owner, read-only rendering of the fleet name: plain, legible
     text so the name is unmistakably visible (not a greyed field). */
  .name-value {
    flex: 1;
    min-width: 0;
    font-size: 0.95rem;
    font-weight: 650;
    color: var(--ink);
    padding: 0.4rem 0;
  }
  .name-value.unnamed {
    color: var(--ink-faint);
    font-weight: 500;
    font-style: italic;
  }
  code {
    flex: 1;
    min-width: 0;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.6rem;
    font-size: 0.82rem;
    font-family: var(--mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    letter-spacing: 0.02em;
  }
  code.revealed {
    word-break: break-all;
    white-space: normal;
  }
  .members {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .members li {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.3rem 0.5rem 0.3rem 0.1rem;
  }
  /* An owner row gets a faint gold edge so the fleet's authority is legible
     at a glance. */
  .members li.owner {
    box-shadow: inset 2px 0 0 var(--c-fleet);
  }
  /* The identity is display-only now: clicking it no longer yanks you out to
     the graph. The explicit "View" button in the actions does that instead. */
  .m-id-wrap {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.2rem 0.5rem;
  }
  .m-avatar {
    font-size: 1.2rem;
  }
  .m-id {
    min-width: 0;
    flex: 1;
  }
  .m-actions {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }
  /* Role controls: one layer at a time. "Up" (promote) is additive, in the
     fleet's green; "down" (withdraw) is a quiet, neutral step-back, so adding
     and removing a layer read as the same kind of action in opposite directions
     rather than promote-vs-danger. */
  .role-btn {
    border: 1px solid var(--line);
    background: var(--surface);
    color: var(--ink-soft);
    border-radius: var(--r-pill);
    padding: 0.22rem 0.6rem;
    font-size: 0.72rem;
    font-weight: 650;
    cursor: pointer;
    transition: border-color 0.12s ease, background 0.12s ease, color 0.12s ease;
  }
  .role-btn.up {
    border-color: var(--c-fleet-soft);
    background: var(--c-fleet-soft);
    color: var(--c-fleet-ink);
  }
  .role-btn.up:hover {
    border-color: var(--c-fleet);
  }
  .role-btn.down:hover {
    border-color: var(--line-strong);
    color: var(--ink);
    background: var(--surface-2);
  }
  .owner-tag {
    font-size: 0.62rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--c-fleet-ink);
    background: var(--c-fleet-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  /* Manager: distinct from the gold owner, in the fleet's green. */
  .mgr-tag {
    font-size: 0.62rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--c-fleet-ink);
    background: var(--c-fleet-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  /* Infra hub: the accent family, so shape reads apart from authority. */
  .hub-tag {
    font-size: 0.62rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  .role-btn.hub-on {
    border-color: var(--accent-soft);
    background: var(--accent-soft);
    color: var(--accent-ink);
  }
  .role-btn.hub-toggle:hover {
    border-color: var(--accent);
  }
  .hub-apply {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex-wrap: wrap;
    margin-top: 0.4rem;
  }
  .hub-redundancy {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.78rem;
    color: var(--ink-soft);
  }
  .hub-redundancy input {
    width: 4.5rem;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    color: var(--ink);
    font: inherit;
    padding: 0.25rem 0.45rem;
  }
  .hint-inline {
    font-size: 0.78rem;
    color: var(--ink-soft);
  }

  .m-name {
    font-size: 0.88rem;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .self-tag {
    font-size: 0.62rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  .m-sub {
    font-size: 0.72rem;
    color: var(--ink-faint);
    font-family: var(--mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .kick {
    flex-shrink: 0;
    border: 1px solid var(--line);
    background: var(--surface);
    color: var(--ink-soft);
    border-radius: var(--r-pill);
    padding: 0.22rem 0.6rem;
    font-size: 0.72rem;
    font-weight: 650;
    cursor: pointer;
    transition: border-color 0.12s ease, color 0.12s ease, background 0.12s ease;
  }
  .kick:hover,
  .kick.armed {
    border-color: oklch(0.7 0.19 14 / 0.5);
    color: var(--danger);
    background: var(--danger-soft);
  }
  .leave-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    margin-top: 0.7rem;
  }
  .leave-row .hint {
    margin: 0;
    flex: 1;
  }
  .leave.armed {
    border-color: oklch(0.7 0.19 14 / 0.5);
    color: var(--danger);
    background: var(--danger-soft);
  }
  .empty-block {
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
    gap: 0.3rem;
    padding: 2rem 1rem;
  }
  .empty-orb {
    font-size: 2.4rem;
    opacity: 0.8;
  }
  .empty-title {
    font-weight: 700;
    font-size: 1rem;
  }

  /* ---- fleet custody (MFA) ---- */
  .mfa-status {
    font-size: 0.8rem;
    border-radius: var(--r-sm);
    padding: 0.45rem 0.6rem;
    margin: 0.5rem 0;
  }
  .mfa-status.on {
    color: var(--accent-ink);
    background: var(--accent-soft);
  }
  .mfa-status.err {
    color: var(--danger);
    background: var(--danger-soft);
  }
  .btn.primary {
    background: var(--accent-soft);
    border-color: var(--line-strong);
    color: var(--accent-ink);
    font-weight: 650;
  }
  .btn.danger {
    color: var(--danger);
    border-color: oklch(0.7 0.19 14 / 0.5);
    background: var(--danger-soft);
  }
  .mfa-disable {
    margin-top: 0.5rem;
  }
  .pc-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.9rem;
    cursor: pointer;
    user-select: none;
  }
  .claim-code-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.5rem;
  }
  .claim-direction {
    margin-top: 0.65rem;
    padding: 0.65rem 0.7rem;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .claim-direction-title {
    color: var(--ink);
    font-size: 0.82rem;
    font-weight: 700;
  }
  .local-claim-id-row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    margin-top: 0.5rem;
  }
  .local-claim-id-row code {
    flex: 1;
    min-width: 0;
    padding: 0.42rem 0.6rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink);
    font-family: var(--mono);
    font-size: 0.88rem;
    font-weight: 650;
    letter-spacing: 0.035em;
    user-select: all;
  }
  .claim-unavailable {
    margin-bottom: 0;
  }
  .mfa-disable-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.5rem;
  }
  .mfa-input {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.6rem;
    font-size: 0.84rem;
    font-family: var(--mono);
    background: var(--surface);
    color: var(--ink);
  }
  .mfa-enroll {
    margin-top: 0.7rem;
    padding: 0.7rem 0.8rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .mfa-enroll-lead {
    margin: 0 0 0.5rem;
    font-size: 0.8rem;
    color: var(--ink-soft);
    line-height: 1.45;
  }
  .mfa-kv {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    margin: 0.35rem 0;
    font-size: 0.78rem;
  }
  .mfa-kv > span {
    min-width: 6.5rem;
    flex-shrink: 0;
    color: var(--ink-faint);
  }
  .mfa-kv code {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.15rem 0.4rem;
    font-size: 0.76rem;
    font-family: var(--mono);
  }
  .mfa-kv code.wrap {
    word-break: break-all;
  }
  .mfa-recovery {
    margin: 0;
    padding-left: 1rem;
    columns: 2;
    list-style: none;
  }
  .mfa-recovery code {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.1rem 0.3rem;
    font-size: 0.74rem;
    font-family: var(--mono);
  }
</style>
