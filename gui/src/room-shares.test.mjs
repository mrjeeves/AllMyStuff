import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("store.svelte.ts", readFileSync(new URL("./store.svelte.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const store = source.statements.find((node) => ts.isClassDeclaration(node) && node.name?.text === "AppStore");
const names = [
  "handleRoomLocal", "handleRoomMessage", "addMyShares", "publishMyShares", "setHostShare",
  "setRoomShareCatalog", "tracksRoomShareCatalog", "hostSharedEntries", "rebroadcastHostShares",
  "presenceAdd", "presenceDrop",
];
const methods = names.map((name) => store.members.find((node) => ts.isMethodDeclaration(node) && node.name.getText(source) === name).getText(source));
const script = ts.transpileModule(`new class { ${methods.join("\n")} }()`, {
  compilerOptions: { target: ts.ScriptTarget.ES2022 },
}).outputText;

test("one host window publishes a complete file list as the room window opens and the main window closes", () => {
  const id = "room:host:test";
  const room = { id, name: "Test room", owner: "host", members: ["host", "guest", "late"] };
  const windows = new Map();
  const broadcasts = [];
  const emitRoomLocal = (event) => {
    for (const window of windows.values()) window.handleRoomLocal(event);
    return Promise.resolve();
  };
  const openWindow = (token, target = null) => {
    const window = Object.assign(vm.runInNewContext(script, {
      canonicalNodeId: (node) => node,
      sameMachine: (a, b) => a === b,
      roomWindowTarget: () => target,
      emitRoomLocal,
      roomSend: async () => 1,
    }), {
      windowToken: token,
      isMainWindow: token === "main",
      localId: "host",
      backendConnected: true,
      rooms: [room],
      roomsJoinedElsewhere: [],
      roomHostShares: {},
      roomMyShares: {},
      roomSharesFromHost: {},
      roomPresence: { [id]: ["guest"] },
      isJoined: (roomId) => target === roomId,
      isMe: (node) => node === "host",
      isRoomHost: (candidate) => candidate.owner === "host",
      machineByAnyId: (node) => ({ label: node, online: true }),
      roomWho: (node) => ({ who: node }),
      callLog() {},
      refreshRoomScope: async () => {},
      wireActiveRoomLegsToMember: async () => {},
      broadcastRoom: (_, message) => broadcasts.push([token, Array.from(message.files, (f) => f.token).sort()]),
    });
    windows.set(token, window);
    return window;
  };
  const receive = (from, kind, files) => {
    for (const window of windows.values()) window.handleRoomMessage(from, { room: id, kind, files });
  };
  const file = (token) => ({ token, name: `${token}.txt`, size: 42 });

  openWindow("main");
  receive("guest", "share_list", [file("guest")]);
  assert.deepEqual(broadcasts.splice(0), [["main", ["guest"]]]);

  const call = openWindow("call", id);
  emitRoomLocal({ token: "call", kind: "join", room: id });
  assert.deepEqual(broadcasts.splice(0), [["call", ["guest"]]]);
  call.addMyShares(id, [file("host")]);
  assert.deepEqual(broadcasts.splice(0), [["call", ["guest", "host"]]]);

  receive("guest", "share_list", [file("replacement")]);
  assert.deepEqual(broadcasts.splice(0), [["call", ["host", "replacement"]]]);
  receive("uninvited", "share_list", [file("uninvited")]);
  assert.deepEqual(broadcasts.splice(0), []);
  receive("late", "join");
  assert.deepEqual(broadcasts.splice(0), [["call", ["host", "replacement"]]]);

  windows.delete("main");
  receive("guest", "leave");
  assert.deepEqual(broadcasts, [["call", ["host"]]]);
});
