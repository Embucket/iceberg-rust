# File Catalog

Filesystem and object-store-backed Iceberg catalog.

## Local Namespace Mutations

For filesystem namespaces containing one ordinary identifier
(`[A-Za-z_][A-Za-z0-9_]*`), CREATE and DROP run as blocking filesystem jobs.
The existing process-local catalog mutation lock is owned by the actual job,
through directory changes and DROP's cache cleanup. Cancelling its asynchronous
waiter does not release that lock while the filesystem job can still mutate.
Later mutations on the same catalog-lock key wait for the job to finish.

Cancellation does not roll back the operation or establish its outcome. Blocking
filesystem IO cannot be forcibly stopped, so a caller's deadline must not be
interpreted as proof that CREATE or DROP did not happen. DROP treats an absent
directory as success and does not follow symlink targets outside the namespace.

Memory/object-store namespaces and other identifier forms retain their existing
behavior. This is not distributed locking, general catalog transaction atomicity,
durable namespace-incarnation fencing, or namespace-property persistence. It does
not fence data writers outside the catalog protocol or another component's late
display-marker writes.

The `cancelled_namespace_*` and
`filesystem_namespace_drop_does_not_follow_nested_symlinks` tests exercise real
blocking jobs, separate catalog instances, encoded roots, populated deletion,
cache cleanup and recreation. Their pauses and waits are bounded.

```sh
cargo test -p iceberg-file-catalog --lib cancelled_namespace
cargo test -p iceberg-file-catalog --lib filesystem_namespace_drop
```
