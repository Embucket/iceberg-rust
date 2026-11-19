# Exactly two existing-property view rename

Status: test-first source only. New controls are authored but uncompiled and
unexecuted; no native or embedding SQL success is claimed.

The proposed fixed-array description accepts exactly two distinct existing keys
with the existing single-value bounds. Its constructor canonicalizes key order.
The V4 kind is `view-rename-property-pair` with exactly two strict descriptors in
`property_replacements`. Recovery derives the entire successor from the complete
Before, destination and these two old-to-new value replacements. There is no
caller After, callback, insertion, removal or postpublication update.

V2 location-only and V3 one-property bytes/semantics remain independent. New
fixtures and controls cover exact full-image/history/reopen, bounds, stale full
Before and either old value, malformed and noncanonical V4 descriptors, extra
edits, V2/V3 golden bytes, mixed terminal decisions, actual worker cancellation,
queued lost response and five-phase owned child-process recovery.

The intended engine owner separately validates `embucket.sql.view-source-sql.v1`
and `embucket.sql.view-options.v1`, generating its clock once and preserving it in
its own intent. Native strings remain opaque, and `embucket.sql.view-definition`
is preserved. SQL rights, account/cache/transaction lifetimes, clock semantics,
engine pin and workflow qualification remain outside this primitive.
