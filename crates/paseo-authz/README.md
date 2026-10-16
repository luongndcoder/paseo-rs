# paseo-authz

Port of `packages/server/src/server/authorization`. Tables come from `tables/{inbound,outbound}.json`, extracted from the TS source by `scripts/extract_tables.py` (rerun after the TS table changes); `build.rs` turns them into sorted static arrays. The exhaustiveness test compares them with `paseo-protocol/schema/session-{inbound,outbound}.json`. Unmapped types deny.
