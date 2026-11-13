import type { FileEvent } from "./types";

export const FILE_CHUNK_BYTES = 40 * 1024;
const UPLOAD_WINDOW_CHUNKS = 32;

type FileReply = Extract<FileEvent, { kind: "ok" | "err" | "metadata" | "exists" }>;
type RequestLease = {
  req: number;
  reply: Promise<FileReply>;
  touch: () => void;
  check: () => void;
  fail: (error: unknown) => void;
};

/** A send acknowledgement only means the mesh accepted a frame. Completion
 * requires the file host's reply, registered before sending the request. */
export class FileRequests {
  private pending = new Map<number, { accept: (reply: FileReply) => void; fail: (error: unknown) => void }>();
  private stopped: Error | null = null;
  private sendFrame: (event: FileEvent) => Promise<void>;
  private next: () => number;
  private timeout: number;

  constructor(send: (event: FileEvent) => Promise<void>, next: () => number, timeout = 60_000) {
    this.sendFrame = send;
    this.next = next;
    this.timeout = timeout;
  }

  begin(timeout = this.timeout): RequestLease {
    if (this.stopped) throw this.stopped;
    const req = this.next();
    let error: Error | null = null;
    let timer: ReturnType<typeof setTimeout>;
    let resolve!: (reply: FileReply) => void;
    let reject!: (error: Error) => void;
    const reply = new Promise<FileReply>((yes, no) => { resolve = yes; reject = no; });
    // Streaming callers inspect failures between chunks before awaiting the
    // final acknowledgement; an early remote error must still be observed.
    void reply.catch(() => {});
    const finish = () => { clearTimeout(timer); this.pending.delete(req); };
    const fail = (reason: unknown) => {
      if (!this.pending.has(req)) return;
      error = reason instanceof Error ? reason : new Error(String(reason));
      finish();
      reject(error);
    };
    const touch = () => {
      if (!this.pending.has(req)) return;
      clearTimeout(timer);
      timer = setTimeout(() => fail(new Error("The remote file operation timed out")), timeout);
    };
    this.pending.set(req, {
      fail,
      accept: (event) => {
        if (event.kind === "err") fail(new Error(event.reason));
        else { finish(); resolve(event); }
      },
    });
    touch();
    return { req, reply, touch, fail, check: () => { if (error) throw error; } };
  }

  accept(event: FileEvent): boolean {
    if (!["ok", "err", "metadata", "exists"].includes(event.kind)) return false;
    const pending = this.pending.get(event.req);
    if (!pending) return false;
    pending.accept(event as FileReply);
    return true;
  }

  async send(event: FileEvent, lease?: RequestLease): Promise<void> {
    if (this.stopped) throw this.stopped;
    lease?.check();
    await this.sendFrame(event);
    lease?.touch();
    lease?.check();
  }

  async request(event: (req: number) => FileEvent, timeout?: number): Promise<FileReply> {
    const lease = this.begin(timeout);
    try { await this.send(event(lease.req), lease); }
    catch (error) { lease.fail(error); }
    return lease.reply;
  }

  close(reason = "The file session ended"): void {
    this.stopped = new Error(reason);
    for (const pending of [...this.pending.values()]) pending.fail(this.stopped);
  }
}

export function* fileChunkRanges(size: number): Generator<[number, number]> {
  if (!Number.isSafeInteger(size) || size < 0) throw new Error("Invalid file size");
  for (let offset = 0; offset < size; offset += FILE_CHUNK_BYTES) {
    yield [offset, Math.min(offset + FILE_CHUNK_BYTES, size)];
  }
}

function encode(bytes: Uint8Array): string {
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}

function missing(error: unknown): boolean {
  const text = String(error).toLowerCase();
  return /\(os error [23]\)/.test(text) || text.includes("not found")
    || text.includes("cannot find") || text.includes("no such file");
}

async function expectOk(request: Promise<FileReply>): Promise<void> {
  if ((await request).kind !== "ok") throw new Error("The file host returned an unexpected reply");
}

/** Stream a browser-selected Blob without reading the whole file into the
 * webview. Publish a verified staging body only after the host acknowledges it.
 * Existing destination files are never opened for append or replacement. */
export async function uploadBrowserFile(
  file: Pick<Blob, "size" | "slice">,
  destination: string,
  requests: FileRequests,
  options: { signal?: AbortSignal; progress?: (written: number) => void; token?: string } = {},
): Promise<void> {
  if (!Number.isSafeInteger(file.size) || file.size < 0) throw new Error("Invalid file size");
  const check = () => options.signal?.throwIfAborted();
  check();
  try {
    const reply = await requests.request((req) => ({ kind: "stat", req, path: destination }));
    if (reply.kind !== "metadata") throw new Error("The file host returned no destination metadata");
    throw new Error("A file with that name already exists at the destination");
  } catch (error) {
    if (!missing(error)) throw error;
  }
  const token = options.token ?? crypto.randomUUID();
  if (!/^[a-zA-Z0-9-]{1,80}$/.test(token)) throw new Error("Invalid upload identity");
  const separator = Math.max(destination.lastIndexOf("/"), destination.lastIndexOf("\\"));
  const staging = destination.slice(0, separator + 1) + `.allmystuff-browser-upload-${token}.part`;
  let owned = false;
  let stream: RequestLease | undefined;
  try {
    check();
    await expectOk(requests.request((req) => ({
      kind: "write", req, path: staging, data: "", append: false, create_new: true, eof: true,
    })));
    owned = true;
    if (file.size > 0) {
      let windowChunks = 0;
      for (const [start, end] of fileChunkRanges(file.size)) {
        check();
        stream ??= requests.begin();
        stream.check();
        const bytes = new Uint8Array(await file.slice(start, end).arrayBuffer());
        if (bytes.length !== end - start) throw new Error("The selected file changed during upload");
        check();
        const checkpoint = ++windowChunks === UPLOAD_WINDOW_CHUNKS || end === file.size;
        await requests.send({
          kind: "write", req: stream.req, path: staging, data: encode(bytes),
          append: true, create_new: false, eof: checkpoint,
        }, stream);
        // Bound the remote write queue as well as the browser's memory.
        // Legacy hosts acknowledge Write at eof, so each small window can
        // wait for disk application without requiring a protocol upgrade.
        if (checkpoint) {
          await expectOk(stream.reply);
          stream = undefined;
          windowChunks = 0;
          options.progress?.(end);
        }
      }
    }
    check();
    const metadata = await requests.request((req) => ({ kind: "stat", req, path: staging }));
    if (metadata.kind !== "metadata" || metadata.entry.dir || metadata.entry.size !== file.size) {
      throw new Error("The remote upload is incomplete; the destination was not changed");
    }
    check();
    await expectOk(requests.request((req) => ({ kind: "rename", req, from: staging, to: destination })));
    owned = false;
  } catch (error) {
    stream?.fail(error);
    if (owned) {
      try { await expectOk(requests.request((req) => ({ kind: "delete", req, path: staging }), 5_000)); }
      catch { throw new Error(`${String(error)}. The partial upload could not be removed: ${staging}`); }
    }
    throw error;
  }
}
