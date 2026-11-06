export interface FilesStorageAttention {
  key: string;
  title: string;
  message: string;
}

type DismissalStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;
const STORAGE_PREFIX = "allmystuff.filesStorageAttention.v1:";

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isBytes(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function canonicalDevice(device: string): string {
  // Presence can append a five-character node suffix to the durable device key.
  return device.replace(/-[0-9a-zA-Z]{5}$/, "");
}

function bytesLabel(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

/** Configured protection and logical usage, not live replicas or disk space.
 * `undefined` means an unreadable response; `null` means confirmed healthy. */
export function classifyFleetStorageAttention(
  status: unknown,
): FilesStorageAttention | null | undefined {
  if (!isRecord(status) || !isRecord(status.plan)
    || !isRecord(status.plan.policy) || !isRecord(status.plan.policy.value)) return undefined;
  const replicas = status.plan.policy.value.replicas;
  const allocations = status.plan.allocations;
  const used = status.logicalUsedBytes;
  const capacity = status.protectedCapacityBytes;
  if (typeof replicas !== "number" || !Number.isInteger(replicas) || replicas < 1
    || !Array.isArray(allocations) || !isBytes(used) || !isBytes(capacity)) return undefined;

  const devices = new Set<string>();
  for (const allocation of allocations) {
    if (!isRecord(allocation) || typeof allocation.enabled !== "boolean"
      || typeof allocation.device !== "string" || !allocation.device.trim()) return undefined;
    if (allocation.enabled) {
      const device = canonicalDevice(allocation.device.trim());
      if (!device) return undefined;
      devices.add(device);
    }
  }

  if (devices.size === 0) {
    return used > 0 ? {
      key: "no-allocation",
      title: "Fleetfiles has no configured storage",
      message: `Fleetfiles contains ${bytesLabel(used)} of logical data, but no fleet storage is configured. Add a storage contribution in Files settings.`,
    } : null;
  }
  if (devices.size < replicas) {
    return {
      key: `insufficient-devices:${replicas}:${devices.size}`,
      title: "More storage devices are needed",
      message: `Your storage plan requests ${replicas} copies on separate devices; ${devices.size} device${devices.size === 1 ? " has" : "s have"} storage configured. Add storage on another device or adjust the number of copies in Files settings.`,
    };
  }
  if (capacity > 0 && used >= capacity) {
    return {
      key: "capacity-full",
      title: "Configured Fleetfiles capacity is full",
      message: `${bytesLabel(used)} of logical data uses all ${bytesLabel(capacity)} of configured protected capacity. Increase storage contributions or adjust the storage plan in Files settings.`,
    };
  }
  const remaining = Math.max(0, capacity - used);
  if (capacity > 0 && remaining / capacity <= 0.15) {
    return {
      key: "capacity-low",
      title: "Fleetfiles storage is running low",
      message: `${bytesLabel(remaining)} of ${bytesLabel(capacity)} configured protected capacity remains. Increase storage contributions or adjust the storage plan in Files settings.`,
    };
  }
  return null;
}

function browserStorage(): DismissalStorage | undefined {
  try {
    return typeof localStorage === "undefined" ? undefined : localStorage;
  } catch {
    return undefined;
  }
}

/** One device-local acknowledgement for the current public fleet/local scope. */
export class FilesStorageAttentionState {
  private storage: DismissalStorage | undefined;
  private scope: string | null = null;
  private attention: FilesStorageAttention | null = null;
  private dismissed: string | null = null;

  constructor(storage: DismissalStorage | undefined = browserStorage()) {
    this.storage = storage;
  }

  get storageKey(): string | null {
    return this.scope === null ? null : STORAGE_PREFIX + encodeURIComponent(this.scope);
  }

  get visible(): FilesStorageAttention | null {
    return this.attention?.key === this.dismissed ? null : this.attention;
  }

  setScope(scope: string | null): void {
    const next = scope?.trim() || null;
    if (next === this.scope) return;
    this.scope = next;
    this.attention = null;
    this.dismissed = null;
    this.syncDismissal();
  }

  update(scope: string | null, status: unknown): void {
    if (this.scope === null || scope !== this.scope) return;
    const next = classifyFleetStorageAttention(status);
    if (next === undefined) return;
    this.attention = next;
    if (next === null || (this.dismissed !== null && this.dismissed !== next.key)) {
      this.clearDismissal();
    }
  }

  dismiss(): void {
    const key = this.storageKey;
    if (key === null || this.attention === null) return;
    this.dismissed = this.attention.key;
    try {
      this.storage?.setItem(key, this.dismissed);
    } catch { /* The acknowledgement still works in memory. */ }
  }

  syncDismissal(): void {
    const key = this.storageKey;
    if (key === null || !this.storage) return;
    try {
      this.dismissed = this.storage.getItem(key);
    } catch { /* A failed read must not undo an in-memory acknowledgement. */ }
  }

  private clearDismissal(): void {
    this.dismissed = null;
    const key = this.storageKey;
    if (key === null) return;
    try {
      this.storage?.removeItem(key);
    } catch { /* Recovery and new reasons still reset the in-memory state. */ }
  }
}
