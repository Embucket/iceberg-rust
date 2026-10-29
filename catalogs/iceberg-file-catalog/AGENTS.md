# File Catalog Guidance

- This crate owns file-catalog metadata lookup and filesystem/object-store
  mutations, not SQL session selection or Rustice display markers.
- Preserve the existing URI/object-path conversion helpers. Tests must derive
  native paths through the configured store rather than assuming URL spelling
  is already a native filesystem path.
- A filesystem mutation's lock must be owned by its blocking job through IO and
  cache cleanup, not only by the cancellable future awaiting its JoinHandle.
- Keep non-filesystem behavior and table/query fast paths unchanged when fixing
  local namespace lifecycle. Process-local locks do not establish distributed
  atomicity or stop external data writers.
- Cancellation tests must pause inside the actual blocking worker, await the
  caller's cancellation, and independently verify the lock is still held. Bound
  both worker pauses and async waits; a single pending poll is not proof.
- Run focused namespace tests and an independent review before finalizing a PR.
