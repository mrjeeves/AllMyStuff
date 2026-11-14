import assert from "node:assert/strict";
import test from "node:test";
import { FILE_CHUNK_BYTES, FileRequests, fileChunkRanges, uploadBrowserFile } from "./files-transfer.ts";

function host(options = {}) {
  const files = new Map();
  const sent = [];
  let next = 0;
  let channel;
  const send = async (event) => {
    sent.push(event);
    if (await options.intercept?.(event, files, channel)) return;
    let reply;
    if (event.kind === "stat") {
      const body = files.get(event.path);
      reply = body ? { kind: "metadata", req: event.req, entry: { name: event.path, dir: false, size: body.length } }
        : { kind: "err", req: event.req, reason: "The system cannot find the file specified. (os error 2)" };
    } else if (event.kind === "write") {
      if (event.create_new && files.has(event.path)) reply = { kind: "err", req: event.req, reason: "already exists" };
      else {
        const data = Buffer.from(event.data, "base64");
        files.set(event.path, event.append ? Buffer.concat([files.get(event.path) ?? Buffer.alloc(0), data]) : data);
        if (event.eof) reply = { kind: "ok", req: event.req };
      }
    } else if (event.kind === "rename") {
      if (files.has(event.to)) reply = { kind: "err", req: event.req, reason: "already exists" };
      else { files.set(event.to, files.get(event.from)); files.delete(event.from); reply = { kind: "ok", req: event.req }; }
    } else if (event.kind === "delete") {
      files.delete(event.path);
      reply = { kind: "ok", req: event.req };
    }
    if (reply) channel.accept(reply);
  };
  channel = new FileRequests(send, () => ++next, 1_000);
  return { channel, files, sent };
}

test("browser uploads read bounded slices and publish only after the final host reply", async () => {
  const remote = host();
  const body = new Uint8Array(FILE_CHUNK_BYTES * 2 + 19).map((_, index) => index % 251);
  const blob = new Blob([body]);
  let largestRead = 0;
  const selected = {
    size: blob.size,
    arrayBuffer: () => { throw new Error("whole-file allocation is forbidden"); },
    slice: (start, end) => { largestRead = Math.max(largestRead, end - start); return blob.slice(start, end); },
  };
  const progress = [];
  await uploadBrowserFile(selected, "C:\\target\\large.bin", remote.channel, { token: "bounded", progress: (bytes) => progress.push(bytes) });
  assert.equal(largestRead, FILE_CHUNK_BYTES);
  assert.deepEqual(remote.files.get("C:\\target\\large.bin"), Buffer.from(body));
  assert.equal(remote.files.size, 1, "the staging body must be gone after publication");
  assert.equal(progress.at(-1), blob.size);
  assert.equal(remote.sent.at(-1).kind, "rename");
});

test("file offsets pass four GiB without narrowing or allocating a large buffer", () => {
  const size = 5 * 1024 ** 3 + 31;
  let previous = 0;
  let count = 0;
  for (const [start, end] of fileChunkRanges(size)) {
    assert.equal(start, previous);
    assert.ok(end > start && end - start <= FILE_CHUNK_BYTES);
    previous = end;
    count++;
  }
  assert.equal(previous, size);
  assert.equal(count, Math.ceil(size / FILE_CHUNK_BYTES));
});

test("an existing destination is never opened for append", async () => {
  const remote = host();
  remote.files.set("/target/file", Buffer.from("keep me"));
  await assert.rejects(uploadBrowserFile(new Blob(["new body"]), "/target/file", remote.channel), /already exists/);
  assert.equal(remote.files.get("/target/file").toString(), "keep me");
  assert.ok(remote.sent.every((event) => event.kind !== "write"));
});

test("a destination appearing before commit is preserved and the private body is removed", async () => {
  const remote = host({ intercept: (event, files) => {
    if (event.kind === "rename") files.set(event.to, Buffer.from("another writer"));
  } });
  await assert.rejects(uploadBrowserFile(new Blob(["our body"]), "/target/file", remote.channel, { token: "collision" }), /already exists/);
  assert.deepEqual([...remote.files.keys()], ["/target/file"]);
  assert.equal(remote.files.get("/target/file").toString(), "another writer");
});

test("a remote error stops further upload chunks and cleans the owned partial", async () => {
  let chunks = 0;
  const remote = host({ intercept: (event, _files, channel) => {
    if (event.kind === "write" && event.append && ++chunks === 2) {
      channel.accept({ kind: "err", req: event.req, reason: "disk full" });
      return true;
    }
  } });
  await assert.rejects(uploadBrowserFile(new Blob([new Uint8Array(FILE_CHUNK_BYTES * 4)]), "/target/file", remote.channel, { token: "failed" }), /disk full/);
  assert.equal(chunks, 2);
  assert.equal(remote.files.size, 0);
  assert.equal(remote.sent.at(-1).kind, "delete");
});

test("cancellation removes a partial body without publishing it", async () => {
  const remote = host();
  const cancel = new AbortController();
  await assert.rejects(uploadBrowserFile(new Blob([new Uint8Array(FILE_CHUNK_BYTES * 33)]), "/target/file", remote.channel, {
    signal: cancel.signal, token: "cancelled", progress: () => cancel.abort(new Error("cancelled")),
  }), /cancelled/);
  assert.equal(remote.files.size, 0);
  assert.equal(remote.sent.filter((event) => event.kind === "write" && event.append).length, 32);
  assert.ok(remote.sent.every((event) => event.kind !== "rename"));
});

test("large uploads wait for remote write checkpoints before sending more windows", async () => {
  let chunks = 0;
  let release;
  let reached;
  const checkpoint = new Promise((resolve) => { reached = resolve; });
  const remote = host({ intercept: (event, files, channel) => {
    if (event.kind !== "write" || !event.append) return;
    chunks++;
    if (chunks === 32) {
      assert.equal(event.eof, true);
      const data = Buffer.from(event.data, "base64");
      files.set(event.path, Buffer.concat([files.get(event.path), data]));
      release = () => channel.accept({ kind: "ok", req: event.req });
      reached();
      return true;
    }
  } });
  const upload = uploadBrowserFile(new Blob([new Uint8Array(FILE_CHUNK_BYTES * 33)]), "/target/file", remote.channel);
  await checkpoint;
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(chunks, 32, "the next window must wait for the remote acknowledgement");
  release();
  await upload;
  assert.equal(chunks, 33);
});

test("zero-byte files still require creation, verification, and publication acknowledgements", async () => {
  const remote = host();
  await uploadBrowserFile(new Blob([]), "/target/empty", remote.channel, { token: "empty" });
  assert.equal(remote.files.get("/target/empty").length, 0);
  assert.equal(remote.sent.filter((event) => event.kind === "write").length, 1);
  assert.equal(remote.sent.at(-1).kind, "rename");
});

test("an incomplete remote body cannot be published as a successful upload", async () => {
  const remote = host({ intercept: (event, _files, channel) => {
    if (event.kind === "write" && event.append) {
      if (event.eof) channel.accept({ kind: "ok", req: event.req });
      return true; // simulate accepted frames that were not applied
    }
  } });
  await assert.rejects(uploadBrowserFile(new Blob(["missing body"]), "/target/file", remote.channel, { token: "short" }), /incomplete/);
  assert.equal(remote.files.size, 0);
  assert.ok(remote.sent.every((event) => event.kind !== "rename"));
});

test("closing a file session rejects pending work instead of leaving a stuck transfer", async () => {
  const channel = new FileRequests(async () => {}, () => 1);
  const request = channel.request((req) => ({ kind: "stat", req, path: "/file" }));
  channel.close("disconnected");
  await assert.rejects(request, /disconnected/);
});

test("private-stage collisions are never cleaned as if they belonged to this upload", async () => {
  const remote = host();
  const existing = "/target/.allmystuff-browser-upload-existing.part";
  remote.files.set(existing, Buffer.from("not ours"));
  await assert.rejects(uploadBrowserFile(new Blob(["new"]), "/target/file", remote.channel, { token: "existing" }), /already exists/);
  assert.equal(remote.files.get(existing).toString(), "not ours");
  assert.ok(remote.sent.every((event) => event.kind !== "delete"));
});
