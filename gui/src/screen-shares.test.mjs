import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { durableGrantCapability, grantPermits, screenShareKey } from "./screen-shares.ts";

const cases = JSON.parse(readFileSync(new URL("../../contract-fixtures/durable_screens.json", import.meta.url)));
for (const c of cases) {
  test(`durable Screens: ${c.name}`, () => {
    assert.equal(grantPermits(c.grant, c.media, c.role, c.capability), c.allowed);
  });
}

test("new durable screen grants use machine scope while other grants remain specific", () => {
  const selected = "workstation-D11F4:screen:80937205";
  assert.equal(durableGrantCapability("display", "provide", selected), "workstation-D11F4:screen");
  assert.equal(durableGrantCapability("video", "provide", "workstation:cam:0"), "workstation:cam:0");
  assert.equal(durableGrantCapability("display", "consume", selected), selected);
});

test("legacy monitors group as one permission without grouping other machines or cameras", () => {
  const legacy = cases[0].grant;
  assert.equal(screenShareKey(legacy), screenShareKey({ ...legacy, capability: "workstation:screen:999" }));
  assert.notEqual(screenShareKey(legacy), screenShareKey({ ...legacy, capability: "other:screen" }));
  assert.equal(screenShareKey({ ...legacy, media: "video", capability: "workstation:cam:0" }), null);
});
