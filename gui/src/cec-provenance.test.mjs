import assert from "node:assert/strict";
import test from "node:test";

import { reconcileCecOnlyCanons } from "./cec-provenance.ts";

test("CEC-only provenance survives peer teardown and catalog pruning", () => {
  assert.deepEqual(reconcileCecOnlyCanons(["tech"], [], []), ["tech"]);
});

test("ordinary mesh provenance makes a former support peer visible", () => {
  assert.deepEqual(reconcileCecOnlyCanons(["tech"], [], ["tech"]), []);
});

test("new CEC peers are marked without disturbing existing provenance", () => {
  assert.deepEqual(
    reconcileCecOnlyCanons(["prior-tech"], ["current-tech"], []),
    ["prior-tech", "current-tech"],
  );
});
