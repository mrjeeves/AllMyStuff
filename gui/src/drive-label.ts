/** The suffix is reserved for the source device's AMS name. */
export function mappedDriveLabel(drive: string, source: string): string {
  let base = drive.trim();
  if (base.endsWith(")")) {
    let depth = 0;
    for (let index = base.length - 1; index >= 0; index--) {
      if (base[index] === ")") depth++;
      else if (base[index] === "(" && --depth === 0) {
        const suffix = base.slice(index + 1, -1);
        if (index > 0 && base[index - 1] === " " && suffix.startsWith("on ") && suffix.slice(3).trim()) {
          base = base.slice(0, index).trimEnd();
        }
        break;
      }
    }
  }
  return `${base || "Drive"} (on ${source.trim() || "Device"})`;
}
