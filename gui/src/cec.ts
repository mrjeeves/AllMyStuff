// The front-end client for the CEC (Critical Error Computing) service — the
// optional account behind the two advertised services, Ask-for-Help and the
// Private Line. Thin wrappers over the Tauri `cec_*` commands; each returns
// `null` off the desktop backend (a plain web preview), so the store can fall
// back to a believable demo, exactly like the rest of `tauri.ts`.
//
// The conventions here mirror `crates/allmystuff-cec/src/convention.rs` — keep
// the two in step.

import { isTauri } from "./tauri";

// --- contract types (mirror crates/allmystuff-cec/src/model.rs) ------------

export type AccountRole = "customer" | "agent";

export interface CecAccount {
  id: string;
  email: string;
  display_name: string;
  roles: AccountRole[];
  device_ids: string[];
}

export type ConciergeTier = "pay_as_you_go" | "priority" | "looked_after";

export interface CecEntitlements {
  hardware: boolean;
  private_line: boolean;
  concierge?: ConciergeTier | null;
}

export interface CecTurn {
  url: string;
  username: string;
  credential: string;
}

export interface CecVenue {
  url?: string | null;
  signaling: string[];
  stun: string[];
  turn: CecTurn[];
}

export interface CecProvision {
  network_id: string;
  label: string;
  venue: CecVenue;
  cec_service_node_id: string;
  auto_approve: boolean;
}

export type SubscriptionStatus = "active" | "cancelled" | "past_due";

export interface CecPrivateLine {
  id: string;
  label: string;
  status: SubscriptionStatus;
  venue: CecVenue;
  monthly_price_cents: number;
}

export type HelpStatus = "queued" | "assigned" | "connected" | "ended" | "cancelled";

export interface CecHelpSession {
  id: string;
  status: HelpStatus;
  network_id: string;
  room_id: string;
  cec_service_node_id: string;
  customer_device_id: string;
  customer_label: string;
  topic?: string | null;
  agent_label?: string | null;
  created_at: number;
}

/** The `{ backend_url, signed_in, account, entitlements, provision }` blob the
 *  Tauri `cec_state` command returns. */
export interface CecSnapshot {
  backend_url: string;
  signed_in: boolean;
  account: CecAccount | null;
  entitlements: CecEntitlements;
  provision: CecProvision | null;
}

export interface SignInStartResult {
  sent: boolean;
  masked_email: string;
}

// --- product copy (verbatim from allmystuff.works/service) -----------------

export const CONCIERGE_TIERS: Record<ConciergeTier, { label: string; price: string; blurb: string }> = {
  pay_as_you_go: {
    label: "Pay as you go",
    price: "$25 / 15 min",
    blurb: "One rate, billed by the quarter hour. No monthly.",
  },
  priority: {
    label: "Priority",
    price: "$19 / mo",
    blurb: "Priority queue, 30 minutes included, $20 / 15 min after.",
  },
  looked_after: {
    label: "Looked after",
    price: "$69 / mo",
    blurb: "Front of the queue, 90 minutes included, scheduled check-ins.",
  },
};

export const PRIVATE_LINE_PRICE = "$10 / month";

// --- conventions (mirror convention.rs) ------------------------------------

export const CEC_NETWORK_PREFIX = "cec-customer-";
export const CEC_NETWORK_LABEL = "CEC";
export const CEC_SERVICE_LABEL = "CEC Service";
const CEC_ROOM_MARKER = "cec-";

export function isCecNetwork(networkId: string | null | undefined): boolean {
  return !!networkId && networkId.startsWith(CEC_NETWORK_PREFIX);
}

/** Whether a room id is a CEC help room (minted by `helpRoomId`). */
export function isHelpRoom(roomId: string): boolean {
  const idx = roomId.lastIndexOf(":");
  return idx >= 0 && roomId.slice(idx + 1).startsWith(CEC_ROOM_MARKER);
}

/** Mint a help room id hosted by `host` (a canonical node id). The `cec-`
 *  marker makes it recognisable as a Concierge session on either side. */
export function helpRoomId(host: string): string {
  const nonce = `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
  return `room:${host}:${CEC_ROOM_MARKER}${nonce}`;
}

/** A human label for a Concierge tier. */
export function conciergeLabel(tier: ConciergeTier | null | undefined): string | null {
  return tier ? CONCIERGE_TIERS[tier].label : null;
}

/** The two product decisions, computed the same way as the Rust side. */
export function wantsCecMesh(e: CecEntitlements | null | undefined): boolean {
  return !!e && (e.hardware || e.private_line || !!e.concierge);
}
export function canAskForHelp(e: CecEntitlements | null | undefined): boolean {
  return !!e && !!e.concierge;
}

// --- Tauri command wrappers ------------------------------------------------

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!isTauri()) return null;
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke(cmd, args)) as T;
}

export const cecState = () => call<CecSnapshot>("cec_state");
export const cecSetBackendUrl = (url: string) => call<CecSnapshot>("cec_set_backend_url", { url });
export const cecStartSignIn = (email: string) => call<SignInStartResult>("cec_start_sign_in", { email });
export const cecVerifySignIn = (
  email: string,
  code: string,
  deviceId?: string,
  deviceLabel?: string,
) => call<CecSnapshot>("cec_verify_sign_in", { email, code, deviceId, deviceLabel });
export const cecRefresh = () => call<CecSnapshot>("cec_refresh");
export const cecSignOut = () => call<CecSnapshot>("cec_sign_out");
export const cecProvisionMesh = (deviceId: string) =>
  call<CecProvision>("cec_provision_mesh", { deviceId });
export const cecRentPrivateLine = (label?: string) =>
  call<CecPrivateLine>("cec_rent_private_line", { label });
export const cecListPrivateLines = () => call<CecPrivateLine[]>("cec_list_private_lines");
export const cecCancelPrivateLine = (id: string) => call<CecSnapshot>("cec_cancel_private_line", { id });
export const cecAskForHelp = (
  networkId: string,
  roomId: string,
  deviceId: string,
  topic?: string,
) => call<CecHelpSession>("cec_ask_for_help", { networkId, roomId, deviceId, topic });
export const cecHelpStatus = (id: string) => call<CecHelpSession>("cec_help_status", { id });
export const cecCancelHelp = (id: string) => call<{ ok: boolean }>("cec_cancel_help", { id });
