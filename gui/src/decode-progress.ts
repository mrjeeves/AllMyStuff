/** A decode-call count is not a deadline: repaired bursts can all be submitted
 * in one JS turn, before WebCodecs may run an output callback. Track actual
 * outstanding work and time without progress. This never controls bitrate. */
export class DecodeProgress {
  private pending = 0;
  private lastProgress = 0;

  submit(now: number) {
    if (this.pending === 0) this.lastProgress = now;
    this.pending++;
  }

  output(now: number) {
    this.pending = Math.max(0, this.pending - 1);
    this.lastProgress = now;
  }

  resume(now: number) { this.lastProgress = now; }

  // The existing once-per-second decoder health sweep supplies the cadence.
  stalled(now: number) { return this.pending > 0 && now - this.lastProgress >= 1000; }
}
