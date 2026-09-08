import type { StreamTune } from "./tauri";

/** Send the latest intent only once a route is active. No timers, and no
 * repeated retunes/encoder restarts on unchanged session snapshots. */
export class ActiveStreamTune {
  private route: string | null = null;
  private signature: string | null = null;

  reset() {
    this.route = null;
    this.signature = null;
  }

  take(route: string | null, active: boolean, tune: StreamTune): boolean {
    if (!route || !active) {
      this.reset();
      return false;
    }
    const values = [tune.maxEdge, tune.bitrate, tune.fps, tune.mode, tune.game];
    const signature = JSON.stringify(values);
    const first = this.route !== route || this.signature === null;
    if (!first && this.signature === signature) return false;
    this.route = route;
    this.signature = signature;
    // An untouched Auto route needs no ask. Clearing previously applied
    // overrides DOES need an ask, including when every field is undefined.
    return !first || values.some((value) => value != null);
  }
}
