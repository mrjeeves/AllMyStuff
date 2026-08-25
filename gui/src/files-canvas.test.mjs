import assert from "node:assert/strict";
import test from "node:test";

import { containingFrame, descendantsOf, desktopColumnPosition, FILE_TILE_SIZES, fileReferenceId, isLegacyAutoRowPlacement, mergeCanvasRecords, nativeFileDisplayName, nativeFileGridMetrics, nativeWindowsLinkExtension, nearestFileTileSize, normalizeFrameNesting, rectsIntersect, resolveDesktopTileCollisions, sharedFilesystemObject, translateCanvasPoint } from "./files-canvas.ts";

test("fleet records converge per entity regardless of merge order", () => {
  const a = { id: "frame:a", kind: "frame", value: { title: "A" }, stamp: { counter: 4, actor: "alpha" } };
  const b = { ...a, value: { title: "B" }, stamp: { counter: 4, actor: "beta" } };
  assert.deepEqual(mergeCanvasRecords([a], [b]).records, mergeCanvasRecords([b], [a]).records);
  assert.equal(mergeCanvasRecords([a], [b]).records[0].value.title, "B");
});

test("a tombstone beats an older layout record", () => {
  const live = { id: "frame:a", kind: "frame", value: {}, stamp: { counter: 2, actor: "a" } };
  const gone = { ...live, value: null, deleted: true, stamp: { counter: 3, actor: "b" } };
  assert.deepEqual(mergeCanvasRecords([live], [gone]).records[0], gone);
});

test("nesting chooses the smallest frame and never chooses a descendant", () => {
  const frames = [
    { id: "outer", title: "", color: "", parentId: null, x: 0, y: 0, width: 500, height: 500 },
    { id: "inner", title: "", color: "", parentId: "outer", x: 50, y: 50, width: 250, height: 250 },
  ];
  assert.equal(containingFrame({ x: 80, y: 80, width: 40, height: 40 }, frames), "inner");
  assert.deepEqual([...descendantsOf("outer", frames)], ["inner"]);
  assert.equal(containingFrame(frames[0], frames, descendantsOf("outer", frames)), null);
});

test("fallback file identity folds Windows paths but not POSIX case", () => {
  assert.equal(fileReferenceId("node:a", "C:\\Users\\Chris\\Doc.txt", "windows"), fileReferenceId("node:a", "c:/users/chris/doc.txt", "windows"));
  assert.notEqual(fileReferenceId("node:a", "/Users/Chris/Doc.txt", "macos"), fileReferenceId("node:a", "/Users/Chris/doc.txt", "macos"));
  assert.notEqual(fileReferenceId("node:a", "/tmp/a", "linux"), fileReferenceId("node:b", "/tmp/a", "linux"));
});

test("Windows shell-link presentation hides only final native suffixes", () => {
  assert.equal(nativeFileDisplayName("AllMyAgents.lnk", "windows"), "AllMyAgents");
  assert.equal(nativeFileDisplayName("CEC Support.LNK", "Windows_NT"), "CEC Support");
  assert.equal(nativeFileDisplayName("Erin's Zoom.url", "windows"), "Erin's Zoom");
  assert.equal(nativeWindowsLinkExtension("Erin's Zoom.URL", "windows"), ".url");
  assert.equal(nativeFileDisplayName("notes.lnk.txt", "windows"), "notes.lnk.txt");
  assert.equal(nativeFileDisplayName("AllMyAgents.lnk", "linux"), "AllMyAgents.lnk");
});

test("file icon sizes use the native Explorer notches and separate grid cells", () => {
  assert.deepEqual(FILE_TILE_SIZES, [32, 48, 96]);
  assert.equal(nearestFileTileSize(31), 32);
  assert.equal(nearestFileTileSize(63), 48);
  assert.equal(nearestFileTileSize(95), 96);
  assert.equal(nearestFileTileSize(Number.NaN), 48);
  assert.deepEqual(nativeFileGridMetrics(48, "windows"), {
    iconSize: 48, tileWidth: 88, tileHeight: 92, columnWidth: 100, rowHeight: 100,
  });
});

test("native size changes resolve top-level collisions without moving framed items", () => {
  const metrics = nativeFileGridMetrics(96, "windows");
  const resolved = resolveDesktopTileCollisions([
    { id: "a", x: 24, y: 24, parentId: null },
    { id: "b", x: 24, y: 100, parentId: null },
    { id: "framed", x: 24, y: 100, parentId: "frame:a" },
  ], metrics);
  assert.deepEqual(resolved.map(({ y }) => y), [24, 252, 100]);
});

test("drag previews and drops share one zoom-aware coordinate conversion", () => {
  assert.deepEqual(translateCanvasPoint({ x: 20, y: 40 }, { x: 100, y: 200 }, { x: 160, y: 170 }, 1.5), { x: 60, y: 20 });
  assert.deepEqual(translateCanvasPoint({ x: 20, y: 40 }, { x: 100, y: 200 }, { x: 160, y: 170 }, 0), { x: 80, y: 10 });
});

test("desktop fallback stays column-major across compact canvas measurement", () => {
  const initial = Array.from({ length: 9 }, (_, index) => desktopColumnPosition(index, 48, 720));
  const measured = Array.from({ length: 9 }, (_, index) => desktopColumnPosition(index, 48, 280));
  assert.deepEqual(measured, initial);
  assert.ok(measured[1].y > measured[0].y);
  assert.ok(measured[8].x > measured[0].x);
});

test("legacy migration matches only the old generated row", () => {
  assert.equal(isLegacyAutoRowPlacement({ id: "a", x: 64, y: 72, parentId: null }), true);
  assert.equal(isLegacyAutoRowPlacement({ id: "b", x: 164, y: 72, parentId: null }), true);
  assert.equal(isLegacyAutoRowPlacement({ id: "manual", x: 165, y: 72, parentId: null }), false);
  assert.equal(isLegacyAutoRowPlacement({ id: "framed", x: 164, y: 72, parentId: "frame" }), false);
});

test("marquee intersection includes partially covered native cells", () => {
  assert.equal(rectsIntersect({ x: 20, y: 20, width: 20, height: 20 }, { x: 35, y: 35, width: 30, height: 30 }), true);
  assert.equal(rectsIntersect({ x: 20, y: 20, width: 10, height: 10 }, { x: 35, y: 35, width: 30, height: 30 }), false);
});

test("concurrent frame reparenting cannot leave a cyclic hierarchy", () => {
  const frames = [
    { id: "a", title: "", color: "", parentId: "b", x: 0, y: 0, width: 100, height: 100 },
    { id: "b", title: "", color: "", parentId: "a", x: 10, y: 10, width: 50, height: 50 },
  ];
  const normalized = normalizeFrameNesting(frames);
  assert.equal(normalized.find((frame) => frame.id === "a").parentId, null);
  assert.equal(normalized.find((frame) => frame.id === "b").parentId, "a");
  assert.deepEqual([...descendantsOf("a", normalized)], ["b"]);
});

test("a missing or tombstoned parent cannot strand a frame", () => {
  const [frame] = normalizeFrameNesting([
    { id: "child", title: "", color: "", parentId: "gone", x: 0, y: 0, width: 50, height: 50 },
  ]);
  assert.equal(frame.parentId, null);
});

test("the sharing map accepts only explicit filesystem object grants", () => {
  assert.deepEqual(
    sharedFilesystemObject({ media: "storage", capability: "node-a:folder:folder-42", label: "Workstation: share Projects" }),
    { sourceNode: "node-a", kind: "folder", objectId: "folder-42", label: "Projects" },
  );
  assert.deepEqual(
    sharedFilesystemObject({ media: "storage", capability: "node-a:file:file-7", label: "share Notes.txt" }),
    { sourceNode: "node-a", kind: "file", objectId: "file-7", label: "Notes.txt" },
  );
  assert.equal(sharedFilesystemObject({ media: "storage", capability: "node-a:disk:backup", label: "Backup" }).kind, "drive");
  assert.equal(sharedFilesystemObject({ media: "storage", capability: "node-a:drive:media", label: "Media" }).kind, "drive");
});

test("the sharing map cannot collide with broad or non-filesystem shares", () => {
  const rejected = [
    { media: "display", capability: "node-a:folder:looks-like-one", label: "Screen" },
    { media: "storage", capability: "node-a:files", label: "All files" },
    { media: "storage", capability: "node-a:storage-in", label: "Storage" },
    { media: "storage", capability: "node-a:folder:", label: "Missing id" },
    { media: "storage", capability: "node-a:folder:id:extra", label: "Too many segments" },
    { media: "storage", capability: ":folder:id", label: "Missing source" },
    { media: "storage", capability: null, label: "Generic storage" },
  ];
  for (const grant of rejected) assert.equal(sharedFilesystemObject(grant), null);
});
