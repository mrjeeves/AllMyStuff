//! The files plane on a phone: a request/reply client over a `files` route.
//!
//! The viewer *asks* ([`FileEvent::List`], [`FileEvent::Read`],
//! [`FileEvent::Write`], [`FileEvent::Mkdir`], [`FileEvent::Rename`],
//! [`FileEvent::Delete`]) and the host *answers* ([`FileEvent::Entries`],
//! [`FileEvent::Chunk`], [`FileEvent::Ok`], [`FileEvent::Err`]). Every request
//! carries a viewer-minted `req` id so replies — especially the multi-`Chunk`
//! body of a `Read` — correlate back. [`FileClient`] owns the `req` counter,
//! builds the request frames, and reassembles in-flight reads into a finished
//! buffer.

use std::collections::HashMap;

use allmystuff_session::{FileEntry, FileEvent, FileFrame};

/// A reply, once [`FileClient::accept`] has interpreted (and, for reads,
/// accumulated) one inbound host frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileReply {
    /// A directory listing. `home` is the host's home dir (sent on every
    /// listing) so the viewer can mark it and start there.
    Listing {
        req: u64,
        path: String,
        home: String,
        entries: Vec<FileEntry>,
    },
    /// A read is partway done — `received` of `total` bytes so far. Emitted on
    /// every non-final chunk so the UI can show progress.
    ReadProgress { req: u64, received: u64, total: u64 },
    /// A read finished: the whole file's bytes, reassembled in order.
    ReadDone { req: u64, data: Vec<u8> },
    /// A `Write`/`Mkdir`/`Rename`/`Delete` succeeded.
    Ok { req: u64 },
    /// The request failed, with the host's reason.
    Err { req: u64, reason: String },
}

#[derive(Debug, Default)]
struct ReadAccum {
    data: Vec<u8>,
    total: u64,
}

/// A files client bound to one `files` route. Cheap; one per open browser.
#[derive(Debug)]
pub struct FileClient {
    route: String,
    seq: u64,
    next_req: u64,
    reads: HashMap<u64, ReadAccum>,
}

impl FileClient {
    pub fn new(route: impl Into<String>) -> Self {
        FileClient {
            route: route.into(),
            seq: 0,
            // req 0 reads as "no request we track" in the wire types, so start
            // at 1 — a real request never collides with that sentinel.
            next_req: 1,
            reads: HashMap::new(),
        }
    }

    pub fn route(&self) -> &str {
        &self.route
    }

    fn alloc(&mut self) -> u64 {
        let r = self.next_req;
        self.next_req += 1;
        r
    }

    fn frame(&mut self, event: FileEvent) -> FileFrame {
        let seq = self.seq;
        self.seq += 1;
        FileFrame::new(self.route.clone(), seq, event)
    }

    /// List a directory (`""` or `"~"` mean the host's home).
    pub fn list(&mut self, path: impl Into<String>) -> (u64, FileFrame) {
        let req = self.alloc();
        (
            req,
            self.frame(FileEvent::List {
                req,
                path: path.into(),
            }),
        )
    }

    /// Read a whole file. The reply arrives as a stream of chunks that
    /// [`FileClient::accept`] reassembles; registers the in-flight read here.
    pub fn read(&mut self, path: impl Into<String>) -> (u64, FileFrame) {
        let req = self.alloc();
        self.reads.insert(req, ReadAccum::default());
        (
            req,
            self.frame(FileEvent::Read {
                req,
                path: path.into(),
            }),
        )
    }

    /// Write one piece of a file. The first piece should set `append = false`
    /// (create/truncate); later pieces `true`. The piece with `eof = true`
    /// commits, and the host answers [`FileEvent::Ok`].
    pub fn write(
        &mut self,
        req: u64,
        path: impl Into<String>,
        data: Vec<u8>,
        append: bool,
        eof: bool,
    ) -> FileFrame {
        self.frame(FileEvent::Write {
            req,
            path: path.into(),
            data,
            append,
            eof,
        })
    }

    /// Allocate a request id to thread through a multi-piece [`FileClient::write`].
    pub fn begin_write(&mut self) -> u64 {
        self.alloc()
    }

    pub fn mkdir(&mut self, path: impl Into<String>) -> (u64, FileFrame) {
        let req = self.alloc();
        (
            req,
            self.frame(FileEvent::Mkdir {
                req,
                path: path.into(),
            }),
        )
    }

    pub fn rename(&mut self, from: impl Into<String>, to: impl Into<String>) -> (u64, FileFrame) {
        let req = self.alloc();
        (
            req,
            self.frame(FileEvent::Rename {
                req,
                from: from.into(),
                to: to.into(),
            }),
        )
    }

    pub fn delete(&mut self, path: impl Into<String>) -> (u64, FileFrame) {
        let req = self.alloc();
        (
            req,
            self.frame(FileEvent::Delete {
                req,
                path: path.into(),
            }),
        )
    }

    /// Interpret one inbound host frame. For a `Read`, accumulates the chunk
    /// and only yields [`FileReply::ReadDone`] on the final piece (with
    /// [`FileReply::ReadProgress`] for the others). Returns `None` for a frame
    /// that isn't this route's, isn't a host *reply*, or is a chunk for a read
    /// we never started (a stray after teardown).
    pub fn accept(&mut self, frame: &FileFrame) -> Option<FileReply> {
        if frame.route != self.route {
            return None;
        }
        match &frame.event {
            FileEvent::Entries {
                req,
                path,
                home,
                entries,
            } => Some(FileReply::Listing {
                req: *req,
                path: path.clone(),
                home: home.clone(),
                entries: entries.clone(),
            }),
            FileEvent::Chunk {
                req,
                data,
                total,
                eof,
            } => {
                let acc = self.reads.get_mut(req)?;
                acc.data.extend_from_slice(data);
                acc.total = *total;
                if *eof {
                    let acc = self.reads.remove(req).unwrap_or_default();
                    Some(FileReply::ReadDone {
                        req: *req,
                        data: acc.data,
                    })
                } else {
                    Some(FileReply::ReadProgress {
                        req: *req,
                        received: acc.data.len() as u64,
                        total: acc.total,
                    })
                }
            }
            FileEvent::Ok { req } => Some(FileReply::Ok { req: *req }),
            FileEvent::Err { req, reason } => Some(FileReply::Err {
                req: *req,
                reason: reason.clone(),
            }),
            // Requests (we send these) and unknown future events aren't replies.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use allmystuff_session::MediaPayload;

    fn val(f: &FileFrame) -> serde_json::Value {
        serde_json::to_value(f).unwrap()
    }

    /// Serialize a host-built frame and decode it the way the media channel
    /// does, so the reply path is exercised end-to-end.
    fn host_frame(route: &str, event: FileEvent) -> FileFrame {
        let mut host = FileClient::new(route);
        let f = host.frame(event);
        match MediaPayload::decode(serde_json::to_value(&f).unwrap()) {
            Some(MediaPayload::File(decoded)) => decoded,
            other => panic!("expected a file frame, got {other:?}"),
        }
    }

    #[test]
    fn requests_get_distinct_ids_starting_at_one() {
        let mut fc = FileClient::new("route:desk:files→phone:files-view:1");
        let (r1, list) = fc.list("");
        let (r2, _read) = fc.read("/etc/hosts");
        assert_eq!(r1, 1);
        assert_eq!(r2, 2);
        let j = val(&list);
        assert_eq!(j["t"], "file");
        assert_eq!(j["kind"], "list");
        assert_eq!(j["req"], 1);
        assert_eq!(j["path"], "");
    }

    #[test]
    fn listing_reply_is_surfaced() {
        let mut fc = FileClient::new("route:r");
        let (req, _) = fc.list("/home/me");
        let reply = host_frame(
            "route:r",
            FileEvent::Entries {
                req,
                path: "/home/me".into(),
                home: "/home/me".into(),
                entries: vec![FileEntry {
                    name: "notes.txt".into(),
                    dir: false,
                    size: 12,
                    modified: None,
                    symlink: false,
                }],
            },
        );
        match fc.accept(&reply) {
            Some(FileReply::Listing {
                path,
                home,
                entries,
                ..
            }) => {
                assert_eq!(path, "/home/me");
                assert_eq!(home, "/home/me");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].name, "notes.txt");
            }
            other => panic!("expected a listing, got {other:?}"),
        }
    }

    #[test]
    fn a_read_reassembles_across_chunks() {
        let mut fc = FileClient::new("route:r");
        let (req, _read) = fc.read("/tmp/f");

        let c1 = host_frame(
            "route:r",
            FileEvent::Chunk {
                req,
                data: b"hello ".to_vec(),
                total: 11,
                eof: false,
            },
        );
        let c2 = host_frame(
            "route:r",
            FileEvent::Chunk {
                req,
                data: b"world".to_vec(),
                total: 11,
                eof: true,
            },
        );

        match fc.accept(&c1) {
            Some(FileReply::ReadProgress {
                received, total, ..
            }) => {
                assert_eq!((received, total), (6, 11));
            }
            other => panic!("expected progress, got {other:?}"),
        }
        match fc.accept(&c2) {
            Some(FileReply::ReadDone { data, .. }) => assert_eq!(data, b"hello world"),
            other => panic!("expected the finished read, got {other:?}"),
        }
        // The read is finished — a stray late chunk for it is ignored.
        let stray = host_frame(
            "route:r",
            FileEvent::Chunk {
                req,
                data: b"!".to_vec(),
                total: 11,
                eof: true,
            },
        );
        assert!(fc.accept(&stray).is_none());
    }

    #[test]
    fn ok_and_err_replies_carry_their_req() {
        let mut fc = FileClient::new("route:r");
        let (req, _) = fc.mkdir("/tmp/new");
        let ok = host_frame("route:r", FileEvent::Ok { req });
        assert_eq!(fc.accept(&ok), Some(FileReply::Ok { req }));

        let (req2, _) = fc.delete("/tmp/locked");
        let err = host_frame(
            "route:r",
            FileEvent::Err {
                req: req2,
                reason: "permission denied".into(),
            },
        );
        assert_eq!(
            fc.accept(&err),
            Some(FileReply::Err {
                req: req2,
                reason: "permission denied".into()
            })
        );
    }

    #[test]
    fn frames_for_another_route_are_ignored() {
        let mut fc = FileClient::new("route:mine");
        let other = host_frame("route:other", FileEvent::Ok { req: 1 });
        assert!(fc.accept(&other).is_none());
    }

    #[test]
    fn a_multi_piece_write_threads_one_req() {
        let mut fc = FileClient::new("route:r");
        let req = fc.begin_write();
        let first = val(&fc.write(req, "/tmp/up", b"part1".to_vec(), false, false));
        let last = val(&fc.write(req, "/tmp/up", b"part2".to_vec(), true, true));
        assert_eq!(first["kind"], "write");
        assert_eq!(first["req"], req);
        assert_eq!(first["append"], false);
        assert_eq!(last["append"], true);
        assert_eq!(last["eof"], true);
    }
}
