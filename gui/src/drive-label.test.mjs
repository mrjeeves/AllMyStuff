import test from "node:test";
import assert from "node:assert/strict";
import { mappedDriveLabel } from "./drive-label.ts";

test("drive names retain custom parentheses and use the current source name once", () => {
  assert.equal(mappedDriveLabel("Photos (raw)", "Laptop"), "Photos (raw) (on Laptop)");
  assert.equal(mappedDriveLabel("Photos (on Old name)", "Laptop"), "Photos (on Laptop)");
  assert.equal(mappedDriveLabel("Photos (on Laptop (work))", "Laptop (work)"), "Photos (on Laptop (work))");
  assert.equal(mappedDriveLabel("  資料  ", "  💻  "), "資料 (on 💻)");
});
