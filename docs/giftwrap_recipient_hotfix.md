# Kind 1059 recipient admission hotfix

The live ingester accepts `--blocked-giftwrap-recipients` or the comma-separated
`PENSIEVE_BLOCKED_GIFTWRAP_RECIPIENTS` environment variable. Values are validated
hex/npub public keys. With no configured keys behavior is unchanged.

For kind 1059 only, any lowercase `p` tag whose second element is a blocked
32-byte hex key rejects the event before dedupe mutation, packing, archive
writing, or downstream indexing. Hex matching is case insensitive. The author
key and relay URL do not affect this rule. Both live and negentropy admission
use the same predicate. Rejection continues ingestion rather than stopping it.

`ingest_policy_dropped_total{source="live|negentropy",reason="giftwrap_recipient"}`
counts rejections without per-event logs or recipient/relay label cardinality.
The startup log reports the number of configured recipients. This is a local
admission filter, not a network subscription exclusion: downloading and SDK
validation still occur. Existing archived events and offline import tools are
not changed. Negentropy may download rejected events again in future cycles.

## Incident baseline

- Installed executable: `/home/pensieve/pensieve/target/release/pensieve-ingest`.
- Observed SHA-256: `bcccb46a23c3a097706ba401c8853054cd14de057fa7a1f6ea411d51ccad615b`.
- Embedded source paths identify `/data/pensieve-build/cutover-553268b-src`.
- Installation mtime: August 31, 2026, 18:19 CEST.
- Server checkout later advanced to `557d241c0e7e42663db59d391da335f79f70e222`.
- Hotfix base: `7aa6a46083bd4211e33dc81c0d85d0d182f71687`. Git comparison shows
  no changes between `553268b`, this base, and `557d241` in Cargo.toml,
  Cargo.lock, or the ingest/core/lake/parquet crates. This is source equivalence,
  not proof of a reproducible build; the original build directory is absent.

Recipient for this incident:
`d0182d1b03c946beeb5d4b6742216c021f38322e8f089bf63722cdddc1f819aa`.
Reported source relays: relay.snort.social and relay.ditto.pub.

## Deployment gate

Run `just precommit`, then build the Linux release candidate. Preserve the
installed executable and configuration before replacing anything. Configure
the recipient with a dedicated systemd environment drop-in, validate candidate
argument parsing, and start ingestion only after candidate verification.
Check the configured-recipient startup count, rejection counters, ordinary
event admission, segment sealing, downstream progress, and disk growth.
Do not delete archives or dedupe records as part of this hotfix.

If the candidate fails, stop ingestion and preserve diagnostics. Restoring the
old binary removes the filter, so do not restart it into the spam stream without
an alternative containment measure.
