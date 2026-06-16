# The CEC service HTTP contract

This is the source-of-truth contract between AllMyStuff (and the headless
`allmystuff-agent`) and the hosted **CEC** — Critical Error Computing —
backend. The Rust types in [`src/model.rs`](src/model.rs) **are** this
contract; [`src/mock.rs`](src/mock.rs) is a faithful reference implementation
of it (run it with `allmystuff-cec-mock`).

The free app needs none of this. An **optional account** unlocks the two
services advertised on allmystuff.works:

- **Concierge** — the *Ask-for-Help* button ("Summon" on the site). A real CEC
  technician, one tap away.
- **Private Line** — "a venue of your own": CEC-hosted signaling/STUN/TURN
  serving only the customer's devices.

## Conventions

- Base URL is configurable in the app (Settings → Account). The reference
  default is `https://api.allmystuff.works`; a local mock is
  `http://127.0.0.1:8787`.
- All bodies are JSON; all field names are `snake_case`.
- Authenticated requests carry `Authorization: Bearer <token>`.
- Errors are a non-2xx status with a body of either `{"error":"message"}` or
  `{"error":{"code":"...","message":"..."}}`. Known codes: `bad_email`,
  `bad_code`, `no_token`, `bad_token`, `not_agent`, `offline`, `taken`,
  `already_done`, `no_help`, `no_mesh`, `no_private_line`, `no_venue`.
- Identity proof is the **email one-time code**. Binding a `device_id` (the
  bare mesh pubkey) associates the device with the account so the backend can
  provision the customer's mesh and pre-trust the CEC Service node. (The
  daemon control socket exposes no signing op, so binding is association, not
  a signed challenge — a signed-challenge upgrade is possible later without a
  wire change.)

## Endpoints

### Auth

| Method | Path | Body | → | Notes |
| --- | --- | --- | --- | --- |
| POST | `/v1/auth/start` | `StartSignIn { email }` | `StartSignInResponse { sent, masked_email }` | Emails a one-time code. The mock also returns `dev_code` and prints it. |
| POST | `/v1/auth/verify` | `VerifySignIn { email, code, device_id?, device_label? }` | `Session { token, account, entitlements }` | Creates the account on first sign-in; binds the device in the same call. |
| GET | `/v1/me` | — | `Me { account, entitlements }` | Bearer. |
| POST | `/v1/auth/signout` | — | `{ ok }` | Bearer. Invalidates the token. |
| POST | `/v1/me/device` | `BindDevice { device_id, label? }` | `Account` | Bearer. Bind/relabel a mesh device. |

### The CEC mesh

| Method | Path | Body | → | Notes |
| --- | --- | --- | --- | --- |
| POST | `/v1/mesh/provision` | `{ device_id }` | `MeshProvision { network_id, label, venue, cec_service_node_id, auto_approve }` | Bearer. Idempotent — the same account always gets the same `cec-customer-<hash>` network and CEC Service node. |
| GET | `/v1/mesh` | — | `MeshProvision` or 404 | Bearer. |

`network_id` is `cec-customer-<hash>` where `<hash>` is a stable, opaque
16-hex digest of the account id (no PII reaches the mesh). The `venue` carries
both inline servers and a live venue-file `url`, so the app adds it as an
ordinary **remote venue**. The single `cec_service_node_id` is the only
non-customer peer that ever appears on this network — every CEC connection
rides it; individual agents live behind the backend.

### Private Line

| Method | Path | Body | → | Notes |
| --- | --- | --- | --- | --- |
| POST | `/v1/private-line` | `RentPrivateLine { label? }` | `PrivateLine { id, label, status, venue, monthly_price_cents }` | Bearer. $10/mo. |
| GET | `/v1/private-line` | — | `PrivateLine[]` | Bearer. |
| DELETE | `/v1/private-line/{id}` | — | `{ ok }` | Bearer. Cancels. |
| GET | `/v1/venues/{token}` | — | `VenueFile` (`allmystuff.venue` envelope) | Public — the live servers for a remote venue. |

### Ask-for-Help — customer

| Method | Path | Body | → | Notes |
| --- | --- | --- | --- | --- |
| POST | `/v1/help` | `AskForHelp { network_id, room_id, device_id, topic? }` | `HelpSession` | Bearer. The app has already minted the host-side help room (`room:{host}:cec-{nonce}`). |
| GET | `/v1/help/{id}` | — | `HelpSession` | Bearer. Poll status: `queued → assigned → connected → ended`. |
| POST | `/v1/help/{id}/cancel` | — | `{ ok }` | Bearer. |

### Agent

| Method | Path | Body | → | Notes |
| --- | --- | --- | --- | --- |
| POST | `/v1/agent/presence` | `SetPresence { online }` | `AgentPresence { online, since }` | Bearer (agent role). |
| GET | `/v1/agent/queue` | — | `HelpSession[]` | Bearer (agent role, online). Oldest first. |
| POST | `/v1/agent/help/{id}/accept` | — | `AgentAssignment { session, venue }` | Bearer (agent). Receive the venue + room to join as the CEC Service node. |
| POST | `/v1/agent/help/{id}/decline` | — | `{ ok }` | Leaves it queued. |
| POST | `/v1/agent/help/{id}/end` | — | `{ ok }` | Ends the session. |

### Mock-only

| Method | Path | Body | → | Notes |
| --- | --- | --- | --- | --- |
| POST | `/v1/dev/grant` | `DevGrant { email, entitlements?, agent }` | `{ ok }` | **Mock only.** Sets entitlements / agent role for local testing. A real backend would not expose this. |
| GET | `/v1/health` | — | `{ ok, service }` | Liveness. |

## The flows

**Customer asks for help**

1. App ensures the CEC mesh exists (`POST /v1/mesh/provision`), joins
   `cec-customer-<hash>` with the returned venue, and pre-approves the CEC
   Service node.
2. App mints a host-side help room on that network and `POST /v1/help`.
3. Backend exposes the queued session to online agents.
4. An agent accepts; the backend bridges them onto the CEC Service node and
   into the room. The customer polls `GET /v1/help/{id}` and sees `assigned`,
   then `connected`, with the agent's name.

**Agent handles a request**

1. `verify` → `online` → `GET /v1/agent/queue`.
2. `accept` returns the venue + room. The agent's live link to the customer is
   the backend's job — it operates the single CEC Service node and bridges the
   agent to it (connections managed by the provider, not the mesh engine).
3. `end` when done.
