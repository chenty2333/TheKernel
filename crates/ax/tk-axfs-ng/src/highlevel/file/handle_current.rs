//! File: reads and writes at the current position.

use super::*;

impl File {
    /// Reads data from the current position, advancing the cursor.
    pub fn read(&self, dst: impl Write + IoBufMut) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at(dst, 0)
        }
    }

    pub fn read_slice(&self, dst: &mut [u8]) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at_slice(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at_slice(dst, 0)
        }
    }

    /// Reads from the current position and advances it by exactly the prefix
    /// accepted by `consume`.
    ///
    /// The position lock remains held across the callback. This is intended
    /// for transfer operations such as sendfile which must not consume source
    /// bytes before the destination accepts them. If the callback fails, the
    /// position is unchanged; a short accepted prefix advances by only that
    /// prefix. Stream nodes without an open-file-description position are not
    /// representable by this transaction and return `InvalidInput`.
    pub fn read_slice_then(
        &self,
        dst: &mut [u8],
        consume: impl FnOnce(&[u8]) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        self.read_slice_at_current_then(dst, |data, _offset| consume(data))
    }

    /// Reads at one frozen current position and commits exactly the prefix
    /// accepted by `consume`.
    ///
    /// The current-position transaction remains owned across `consume`, but
    /// the small position lock is released first. The callback receives the
    /// frozen offset so a higher layer can implement a same-description
    /// transfer through positioned backend I/O without recursively acquiring
    /// this transaction.
    pub fn read_slice_at_current_then(
        &self,
        dst: &mut [u8],
        consume: impl FnOnce(&[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        self.read_slice_at_current_checked_then(dst, |_| Ok(()), consume)
    }

    /// Runs a short, nonblocking callback against one frozen current position.
    ///
    /// This is a generic admission primitive for higher layers which need the
    /// exact current offset without performing backend I/O or advancing it.
    pub fn with_current_position<T>(
        &self,
        inspect: impl FnOnce(u64) -> axio::Result<T>,
    ) -> axio::Result<T> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        inspect(offset)
    }

    /// Runs one complete operation against a frozen current position and
    /// commits its accepted prefix exactly once.
    ///
    /// `operation` receives the initial position and must use positioned I/O;
    /// recursively calling a current-position method on this file would try to
    /// acquire the same transaction again. `max_advance` is validated before
    /// the callback can mutate external state, while the returned advance is
    /// checked against that bound before the cursor is committed. An error
    /// leaves the cursor unchanged.
    pub fn with_current_position_transaction<T>(
        &self,
        max_advance: usize,
        operation: impl FnOnce(u64) -> axio::Result<(T, usize)>,
    ) -> axio::Result<T> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        let max_advance = u64::try_from(max_advance).map_err(|_| VfsError::InvalidInput)?;
        offset
            .checked_add(max_advance)
            .ok_or(VfsError::InvalidInput)?;

        let (value, advance) = operation(offset)?;
        let advance = u64::try_from(advance).map_err(|_| VfsError::InvalidInput)?;
        if advance > max_advance {
            return Err(VfsError::InvalidInput);
        }
        *pos.lock() = offset.checked_add(advance).ok_or(VfsError::InvalidInput)?;
        Ok(value)
    }

    /// Nonblocking counterpart of [`Self::with_current_position_transaction`].
    ///
    /// A caller implementing Linux RWF_NOWAIT must not wait merely to acquire
    /// the shared OFD cursor. `Ok(None)` reports that either cursor lock was
    /// contested before any provider operation or position update occurred.
    pub fn try_with_current_position_transaction<T>(
        &self,
        max_advance: usize,
        operation: impl FnOnce(u64) -> axio::Result<(T, usize)>,
    ) -> axio::Result<Option<T>> {
        let Some(_transaction) = self.position_transaction.try_lock() else {
            return Ok(None);
        };
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let Some(mut position) = pos.try_lock() else {
            return Ok(None);
        };
        let offset = *position;
        let max_advance = u64::try_from(max_advance).map_err(|_| VfsError::InvalidInput)?;
        offset
            .checked_add(max_advance)
            .ok_or(VfsError::InvalidInput)?;

        let (value, advance) = operation(offset)?;
        let advance = u64::try_from(advance).map_err(|_| VfsError::InvalidInput)?;
        if advance > max_advance {
            return Err(VfsError::InvalidInput);
        }
        *position = offset.checked_add(advance).ok_or(VfsError::InvalidInput)?;
        Ok(Some(value))
    }

    /// Reads at one frozen current position after a caller-supplied admission
    /// check, then commits exactly the prefix accepted by `consume`.
    ///
    /// `admit` runs while the current-position transaction is held but before
    /// backend read side effects. It must be short and nonblocking. This keeps
    /// higher-layer range policy outside axfs while giving that policy the same
    /// position snapshot used by the eventual read.
    pub fn read_slice_at_current_checked_then(
        &self,
        dst: &mut [u8],
        admit: impl FnOnce(u64) -> axio::Result<()>,
        consume: impl FnOnce(&[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        let mut state = ();
        self.read_slice_at_current_checked_with(
            dst,
            &mut state,
            |_state, offset| admit(offset),
            |_state, data, offset| consume(data, offset),
        )
    }

    /// Stateful form of [`read_slice_at_current_checked_then`](Self::read_slice_at_current_checked_then).
    ///
    /// Both phases receive the same caller-owned state sequentially. This is
    /// useful when admission and consumption operate on one destination
    /// transaction which cannot be mutably captured by two closures at once.
    pub fn read_slice_at_current_checked_with<S>(
        &self,
        dst: &mut [u8],
        state: &mut S,
        admit: impl FnOnce(&mut S, u64) -> axio::Result<()>,
        consume: impl FnOnce(&mut S, &[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        admit(state, offset)?;
        let read = self.read_at_slice(dst, offset)?;
        // Reject an impossible position before the destination callback can
        // mutate externally visible state.
        offset
            .checked_add(read as u64)
            .ok_or(VfsError::InvalidInput)?;
        if read == 0 {
            return Ok(0);
        }
        let consumed = consume(state, &dst[..read], offset)?;
        if consumed > read {
            return Err(VfsError::InvalidInput);
        }
        *pos.lock() = offset
            .checked_add(consumed as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(consumed)
    }

    /// Runs one positioned write callback at a frozen current position and
    /// advances the cursor by exactly the prefix it reports as committed.
    ///
    /// The callback owns the actual write so an embedding layer can perform
    /// policy admission and positioned backend I/O without recursively taking
    /// this transaction. It must not block while holding unrelated endpoint
    /// transactions.
    pub fn write_slice_at_current_then(
        &self,
        src: &[u8],
        write: impl FnOnce(&[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        offset
            .checked_add(src.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        if src.is_empty() {
            return Ok(0);
        }
        let written = write(src, offset)?;
        if written > src.len() {
            return Err(VfsError::InvalidInput);
        }
        *pos.lock() = offset
            .checked_add(written as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(written)
    }

    pub fn read_vectored_slice(&self, dst: &mut [&mut [u8]]) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at_vectored_slice(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at_vectored_slice(dst, 0)
        }
    }

    /// Writes data using an explicit placement decision.
    ///
    /// `placement` is consumed as the decision for this operation and does
    /// not consult the mutable default append status. Nodes marked
    /// [`NodeFlags::POSITIONED_APPEND`] retain their special ordinary-write
    /// behavior: [`WritePlacement::End`] uses and advances their current
    /// position instead of invoking the inode append operation.
    pub fn write_with_placement_and_admission(
        &self,
        mut src: impl Read + IoBuf,
        placement: WritePlacement,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            if placement == WritePlacement::Current
                || self
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND)
            {
                let offset = *pos.lock();
                let requested = src.remaining();
                offset
                    .checked_add(requested as u64)
                    .ok_or(VfsError::InvalidInput)?;
                let allowed = admit(offset, requested)?;
                if allowed > requested {
                    return Err(VfsError::InvalidInput);
                }
                let mut admitted = (&mut src).take(allowed as u64);
                let written = self.write_at(&mut admitted, offset)?;
                if written > allowed {
                    return Err(VfsError::InvalidInput);
                }
                *pos.lock() = offset
                    .checked_add(written as u64)
                    .ok_or(VfsError::InvalidInput)?;
                Ok(written)
            } else {
                self.write_at_end_with_admission_and_new_end(src, admit)
                    .map(|(written, new_end)| {
                        *pos.lock() = new_end;
                        written
                    })
            }
        } else {
            let requested = src.remaining();
            let allowed = admit(0, requested)?;
            if allowed > requested {
                return Err(VfsError::InvalidInput);
            }
            let mut admitted = (&mut src).take(allowed as u64);
            self.write_at(&mut admitted, 0)
        }
    }

    /// Writes data using an explicit placement decision.
    pub fn write_with_placement(
        &self,
        src: impl Read + IoBuf,
        placement: WritePlacement,
    ) -> axio::Result<usize> {
        self.write_with_placement_and_admission(src, placement, |_offset, requested| Ok(requested))
    }

    /// Writes a byte slice using an explicit placement decision.
    pub fn write_slice_with_placement(
        &self,
        src: &[u8],
        placement: WritePlacement,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if placement == WritePlacement::Current
                || self
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND)
            {
                self.write_at_slice(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            } else {
                self.write_at_end_with_new_end(src)
                    .map(|(written, new_end)| {
                        *pos = new_end;
                        written
                    })
            }
        } else {
            self.write_at_slice(src, 0)
        }
    }

    /// Writes vectored input using an explicit placement decision.
    pub fn write_vectored_slice_with_placement(
        &self,
        src: &[&[u8]],
        placement: WritePlacement,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if placement == WritePlacement::Current
                || self
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND)
            {
                self.write_at_vectored_slice(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            } else {
                self.write_vectored_at_end_with_new_end(src)
                    .map(|(written, new_end)| {
                        if let Some(new_end) = new_end {
                            *pos = new_end;
                        }
                        written
                    })
            }
        } else {
            self.write_at_vectored_slice(src, 0)
        }
    }

    pub(super) fn default_write_placement(&self) -> WritePlacement {
        if self.append_enabled() {
            WritePlacement::End
        } else {
            WritePlacement::Current
        }
    }

    /// Writes data using the file's mutable default append status.
    pub fn write(&self, src: impl Read + IoBuf) -> axio::Result<usize> {
        self.write_with_placement(src, self.default_write_placement())
    }

    /// Writes a byte slice using the file's mutable default append status.
    pub fn write_slice(&self, src: &[u8]) -> axio::Result<usize> {
        self.write_slice_with_placement(src, self.default_write_placement())
    }

    /// Writes vectored input using the file's mutable default append status.
    pub fn write_vectored_slice(&self, src: &[&[u8]]) -> axio::Result<usize> {
        self.write_vectored_slice_with_placement(src, self.default_write_placement())
    }

    /// Flushes any internally buffered data. Currently a no-op.
    pub fn flush(&self) -> axio::Result {
        self.access(FileFlags::empty())?;
        Ok(())
    }
}
