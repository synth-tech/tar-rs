use std::cell::{RefCell, Cell};
use std::cmp;
use std::fs;
use std::io::prelude::*;
use std::io::{self, SeekFrom};
use std::marker;
use std::mem;
use std::path::{Path, Component};

use entry::EntryFields;
use error::TarError;
use other;
use {Entry, Header};

macro_rules! try_iter {
    ($me:expr, $e:expr) => (match $e {
        Ok(e) => e,
        Err(e) => { $me.done = true; return Some(Err(e)) }
    })
}

/// A top-level representation of an archive file.
///
/// This archive can have an entry added to it and it can be iterated over.
pub struct Archive<R: ?Sized + Read> {
    inner: ArchiveInner<R>,
}

struct ArchiveInner<R: ?Sized> {
    pos: Cell<u64>,
    obj: RefCell<::AlignHigher<R>>,
}

/// An iterator over the entries of an archive.
///
/// Requires that `R` implement `Seek`.
pub struct Entries<'a, R: 'a + Read> {
    fields: EntriesFields<'a>,
    _ignored: marker::PhantomData<&'a Archive<R>>,
}

struct EntriesFields<'a> {
    // Need a version with Read + Seek so we can call _seek
    archive: &'a Archive<ReadAndSeek + 'a>,
    // ... but we also need a literal Read so we can call _next_entry
    archive_read: &'a Archive<Read + 'a>,
    done: bool,
    offset: u64,
}

/// An iterator over the entries of an archive.
///
/// Does not require that `R` implements `Seek`, but each entry must be
/// processed before the next.
pub struct EntriesMut<'a, R: 'a + Read> {
    fields: EntriesMutFields<'a>,
    _ignored: marker::PhantomData<&'a Archive<R>>,
}

struct EntriesMutFields<'a> {
    archive: &'a Archive<Read + 'a>,
    next: u64,
    done: bool,
}

impl<R: Read> Archive<R> {
    /// Create a new archive with the underlying object as the reader.
    pub fn new(obj: R) -> Archive<R> {
        Archive {
            inner: ArchiveInner {
                obj: RefCell::new(::AlignHigher(0, obj)),
                pos: Cell::new(0),
            },
        }
    }

    /// Returns the current file position
    pub fn raw_file_position(&self) -> u64 {
        self.inner.pos.get()
    }

    /// Unwrap this archive, returning the underlying object.
    pub fn into_inner(self) -> R {
        self.inner.obj.into_inner().1
    }
}

impl<R: Seek + Read> Archive<R> {
    /// Construct an iterator over the entries of this archive.
    ///
    /// This function can return an error if any underlying I/O operation fails
    /// while attempting to construct the iterator.
    ///
    /// Additionally, the iterator yields `io::Result<Entry>` instead of `Entry`
    /// to handle invalid tar archives as well as any intermittent I/O error
    /// that occurs.
    pub fn entries(&self) -> io::Result<Entries<R>> {
        let me: &Archive<ReadAndSeek> = self;
        let me2: &Archive<Read> = self;
        me._entries(me2).map(|fields| {
            Entries { fields: fields, _ignored: marker::PhantomData }
        })
    }
}

trait ReadAndSeek: Read + Seek {}
impl<R: Read + Seek> ReadAndSeek for R {}

impl<'a> Archive<ReadAndSeek + 'a> {
    fn _entries<'b>(&'b self, read: &'b Archive<Read + 'a>)
                    -> io::Result<EntriesFields<'b>> {
        try!(self._seek(0));
        Ok(EntriesFields {
            archive: self,
            archive_read: read,
            done: false,
            offset: 0,
        })
    }

    fn _seek(&self, pos: u64) -> io::Result<()> {
        if self.inner.pos.get() == pos {
            return Ok(())
        }
        try!(self.inner.obj.borrow_mut().seek(SeekFrom::Start(pos)));
        self.inner.pos.set(pos);
        Ok(())
    }
}

impl<R: Read> Archive<R> {
    /// Construct an iterator over the entries in this archive.
    ///
    /// While similar to the `entries` iterator, this iterator does not require
    /// that `R` implement `Seek` and restricts the iterator to processing only
    /// one entry at a time in a streaming fashion.
    ///
    /// Note that care must be taken to consider each entry within an archive in
    /// sequence. If entries are processed out of sequence (from what the
    /// iterator returns), then the contents read for each entry may be
    /// corrupted.
    pub fn entries_mut(&mut self) -> io::Result<EntriesMut<R>> {
        let me: &mut Archive<Read> = self;
        me._entries_mut().map(|fields| {
            EntriesMut { fields: fields, _ignored: marker::PhantomData }
        })
    }

    /// Unpacks the contents tarball into the specified `dst`.
    ///
    /// This function will iterate over the entire contents of this tarball,
    /// extracting each file in turn to the location specified by the entry's
    /// path name.
    ///
    /// This operation is relatively sensitive in that it will not write files
    /// outside of the path specified by `into`. Files in the archive which have
    /// a '..' in their path are skipped during the unpacking process.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::fs::File;
    /// use tar::Archive;
    ///
    /// let mut ar = Archive::new(File::open("foo.tar").unwrap());
    /// ar.unpack("foo").unwrap();
    /// ```
    pub fn unpack<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<()> {
        let me: &mut Archive<Read> = self;
        me._unpack(dst.as_ref())
    }
}

impl<'a> Archive<Read + 'a> {
    fn _entries_mut(&mut self) -> io::Result<EntriesMutFields> {
        if self.inner.pos.get() != 0 {
            return Err(other("cannot call entries_mut unless archive is at \
                              position 0"))
        }
        Ok(EntriesMutFields {
            archive: self,
            done: false,
            next: 0,
        })
    }

    fn _unpack(&mut self, dst: &Path) -> io::Result<()> {
        'outer: for entry in try!(self._entries_mut()) {
            // TODO: although it may not be the case due to extended headers
            // and GNU extensions, assume each entry is a file for now.
            let file = try!(entry.map_err(|e| {
                TarError::new("failed to iterate over archive", e)
            }));

            // Notes regarding bsdtar 2.8.3 / libarchive 2.8.3:
            // * Leading '/'s are trimmed. For example, `///test` is treated as
            //   `test`.
            // * If the filename contains '..', then the file is skipped when
            //   extracting the tarball.
            // * '//' within a filename is effectively skipped. An error is
            //   logged, but otherwise the effect is as if any two or more
            //   adjacent '/'s within the filename were consolidated into one
            //   '/'.
            //
            // Most of this is handled by the `path` module of the standard
            // library, but we specially handle a few cases here as well.

            let mut file_dst = dst.to_path_buf();
            {
                let path = try!(file.header.path().map_err(|e| {
                    TarError::new("invalid path in entry header", e)
                }));
                for part in path.components() {
                    match part {
                        // Leading '/' characters, root paths, and '.'
                        // components are just ignored and treated as "empty
                        // components"
                        Component::Prefix(..) |
                        Component::RootDir |
                        Component::CurDir => continue,

                        // If any part of the filename is '..', then skip over
                        // unpacking the file to prevent directory traversal
                        // security issues.  See, e.g.: CVE-2001-1267,
                        // CVE-2002-0399, CVE-2005-1918, CVE-2007-4131
                        Component::ParentDir => continue 'outer,

                        Component::Normal(part) => file_dst.push(part),
                    }
                }
            }

            // Skip cases where only slashes or '.' parts were seen, because
            // this is effectively an empty filename.
            if *dst == *file_dst {
                continue
            }

            if let Some(parent) = file_dst.parent() {
                try!(fs::create_dir_all(&parent).map_err(|e| {
                    TarError::new(&format!("failed to create `{}`",
                                           parent.display()), e)
                }));
            }
            try!(file.into_entry::<fs::File>().unpack(&file_dst).map_err(|e| {
                TarError::new(&format!("failed to unpacked `{}`",
                                       file_dst.display()), e)
            }));
        }
        Ok(())
    }

    fn _skip(&self, mut amt: u64) -> io::Result<()> {
        let mut buf = [0u8; 4096 * 8];
        while amt > 0 {
            let n = cmp::min(amt, buf.len() as u64);
            let n = try!((&self.inner).read(&mut buf[..n as usize]));
            if n == 0 {
                return Err(other("unexpected EOF during skip"))
            }
            amt -= n as u64;
        }
        Ok(())
    }

    // Assumes that the underlying reader is positioned at the start of a valid
    // header to parse.
    fn _next_entry(&self,
                   offset: &mut u64,
                   read_at: Box<Fn(u64, &mut [u8]) -> io::Result<usize> + 'a>)
                   -> io::Result<Option<EntryFields>> {
        // If we have 2 or more sections of 0s, then we're done!
        let mut chunk = [0; 512];
        try!(read_all(&mut &self.inner, &mut chunk));
        *offset += 512;
        // A block of 0s is never valid as a header (because of the checksum),
        // so if it's all zero it must be the first of the two end blocks
        if chunk.iter().all(|i| *i == 0) {
            try!(read_all(&mut &self.inner, &mut chunk));
            *offset += 512;
            return if chunk.iter().all(|i| *i == 0) {
                Ok(None)
            } else {
                Err(other("found block of 0s not followed by a second \
                           block of 0s"))
            }
        }

        let sum = chunk[..148].iter().map(|i| *i as u32).fold(0, |a, b| a + b) +
                  chunk[156..].iter().map(|i| *i as u32).fold(0, |a, b| a + b) +
                  32 * 8;

        let header: Header = unsafe { mem::transmute(chunk) };
        let ret = EntryFields {
            pos: 0,
            size: try!(header.size()),
            header: header,
            read_at: read_at,
        };

        // Make sure the checksum is ok
        let cksum = try!(ret.header.cksum());
        if sum != cksum {
            return Err(other("archive header checksum mismatch"))
        }

        // Figure out where the next entry is
        let size = (ret.size + 511) & !(512 - 1);
        *offset += size;

        return Ok(Some(ret));
    }
}

impl<'a, R: ?Sized + Read> Read for &'a ArchiveInner<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.obj.borrow_mut().read(into).map(|i| {
            self.pos.set(self.pos.get() + i as u64);
            i
        })
    }
}

impl<'a, R: Seek + Read> Iterator for Entries<'a, R> {
    type Item = io::Result<Entry<'a, R>>;

    fn next(&mut self) -> Option<io::Result<Entry<'a, R>>> {
        self.fields.next().map(|result| {
            result.map(|fields| fields.into_entry())
        })
    }
}

impl<'a> Iterator for EntriesFields<'a> {
    type Item = io::Result<EntryFields<'a>>;

    fn next(&mut self) -> Option<io::Result<EntryFields<'a>>> {
        // If we hit a previous error, or we reached the end, we're done here
        if self.done {
            return None
        }

        // Seek to the start of the next header in the archive
        try_iter!(self, self.archive._seek(self.offset));

        let offset = self.offset;
        let archive = self.archive;
        let read_at = Box::new(move |at, buf: &mut [u8]| {
            try!(archive._seek(offset + 512 + at));
            (&archive.inner).read(buf)
        });

        // Parse the next entry header
        let archive = self.archive_read;
        match try_iter!(self, archive._next_entry(&mut self.offset, read_at)) {
            Some(f) => Some(Ok(f)),
            None => { self.done = true; None }
        }
    }
}

impl<'a, R: Read> Iterator for EntriesMut<'a, R> {
    type Item = io::Result<Entry<'a, R>>;

    fn next(&mut self) -> Option<io::Result<Entry<'a, R>>> {
        self.fields.next().map(|result| {
            result.map(|fields| fields.into_entry())
        })
    }
}

impl<'a> Iterator for EntriesMutFields<'a> {
    type Item = io::Result<EntryFields<'a>>;

    fn next(&mut self) -> Option<io::Result<EntryFields<'a>>> {
        // If we hit a previous error, or we reached the end, we're done here
        if self.done {
            return None
        }

        // Seek to the start of the next header in the archive
        let delta = self.next - self.archive.inner.pos.get();
        try_iter!(self, self.archive._skip(delta));

        // no need to worry about the position because this reader can't seek
        let archive = self.archive;
        let read_at = Box::new(move |_pos, buf: &mut [u8]| {
            (&archive.inner).read(buf)
        });

        // Parse the next entry header
        match try_iter!(self, self.archive._next_entry(&mut self.next, read_at)) {
            Some(f) => Some(Ok(f)),
            None => { self.done = true; None }
        }
    }
}

fn read_all<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<()> {
    let mut read = 0;
    while read < buf.len() {
        match try!(r.read(&mut buf[read..])) {
            0 => return Err(other("failed to read entire block")),
            n => read += n,
        }
    }
    Ok(())
}
