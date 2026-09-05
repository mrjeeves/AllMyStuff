/** Local monotonic timings only: these gaps locate a stall; they are NOT
 * end-to-end latency or an input to quality adaptation. Constant-size state,
 * with no frame payloads retained and no per-frame diagnostic IPC. */
export class VideoStageStats {
  private since: number;
  private stages: Record<"input" | "decoded" | "painted", {
    count: number; last: number; gap: number;
  }>;

  constructor(now: number) {
    this.since = now;
    this.stages = {
      input: { count: 0, last: now, gap: 0 },
      decoded: { count: 0, last: now, gap: 0 },
      painted: { count: 0, last: now, gap: 0 },
    };
  }

  record(stage: "input" | "decoded" | "painted", now: number) {
    const s = this.stages[stage];
    s.count++;
    s.gap = Math.max(s.gap, now - s.last);
    s.last = now;
  }

  snapshot(now: number) {
    const elapsedMs = now - this.since;
    const read = (stage: "input" | "decoded" | "painted") => {
      const s = this.stages[stage];
      const result = {
        frames: s.count,
        maxGapMs: Math.max(s.gap, now - s.last),
      };
      s.count = 0;
      s.gap = 0;
      return result;
    };
    this.since = now;
    return { elapsedMs, input: read("input"), decoded: read("decoded"), painted: read("painted") };
  }
}
