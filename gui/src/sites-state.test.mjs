import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

function createStore(boundaries) {
  const source = ts.createSourceFile("store.svelte.ts", readFileSync(new URL("./store.svelte.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
  const store = source.statements.find((node) => ts.isClassDeclaration(node) && node.name?.text === "AppStore");
  const names = ["deviceServices", "deviceExposed", "applySiteState", "loadSites", "mappingFromInfo"];
  const methods = names.map((name) => store.members.find((node) => ts.isMethodDeclaration(node) && node.name.getText(source) === name).getText(source));
  const script = ts.transpileModule(`new class { ${methods.join("\n")} }()`, {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  return Object.assign(vm.runInNewContext(script, {
    isTauri: () => true,
    sitePort: (id) => Number(id.split(":")[1]),
    ...boundaries,
  }), {
    siteStateRevision: 0,
    myListening: [],
    exposedSites: {},
    siteMappings: [],
    isMe: (node) => node === "host",
    machineByAnyId: () => undefined,
    syncLocalSites() {},
  });
}

test("Sites shows saved exposure before the listening-service scan finishes", async () => {
  let finishScan;
  const store = createStore({
    siteScan: () => new Promise((resolve) => { finishScan = resolve; }),
    siteExposed: async () => ({ "tcp:5432": "DaVinci Resolve Project Server" }),
    siteMappings: async () => [{ node: "server", port: 5432, localPort: 5432, active: false, remembered: true }],
  });
  const loading = store.loadSites();
  await new Promise(setImmediate);
  assert.equal(store.deviceServices("host")[0]?.name, "DaVinci Resolve Project Server");
  assert.equal(store.siteMappings[0]?.localPort, 5432);
  assert.equal(store.siteMappings[0]?.active, false);
  assert.equal(store.siteMappings[0]?.remembered, true);
  finishScan([]);
  await loading;
  assert.equal(store.deviceServices("host")[0]?.port, 5432);
});

test("Sites keeps a newer node snapshot when an earlier settings read completes", async () => {
  let finishExposure;
  const store = createStore({
    siteScan: async () => [],
    siteExposed: () => new Promise((resolve) => { finishExposure = resolve; }),
    siteMappings: async () => [],
  });
  const loading = store.loadSites();
  store.siteStateRevision++;
  store.applySiteState({ "tcp:5432": "DaVinci Resolve Project Server" }, []);
  finishExposure({});
  await loading;
  assert.equal(store.deviceServices("host")[0]?.name, "DaVinci Resolve Project Server");
});
