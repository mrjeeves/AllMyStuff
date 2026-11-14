type Entry = {
  id: string;
  path: string;
  computerNode?: boolean;
  virtualItem?: boolean;
  logicalPath?: string;
  logicalMaterialized?: boolean;
  binding: { kind: string };
};

// Logical metadata is enough to draw an icon, but not to invoke the OS Shell
// or a preview reader. Opening it first resolves its on-demand content.
export function nativeEntryPath(entry: Entry): string | null {
  return entry.binding.kind === "local" && !entry.computerNode && !entry.virtualItem
    && (!entry.logicalPath || entry.logicalMaterialized === true)
    ? entry.path : null;
}

export function createFileOpener<T extends Entry>(
  busy: (id: string, value: boolean) => void,
  failed: (entry: T, error: unknown) => void,
) {
  const pending = new Map<string, Promise<void>>();
  return (entry: T, action: () => Promise<void>): Promise<void> => {
    const existing = pending.get(entry.id);
    if (existing) return existing;
    busy(entry.id, true);
    const operation = Promise.resolve().then(action)
      .catch((error) => { failed(entry, error); })
      .finally(() => { pending.delete(entry.id); busy(entry.id, false); });
    pending.set(entry.id, operation);
    return operation;
  };
}
