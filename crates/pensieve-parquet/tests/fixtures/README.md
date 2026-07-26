# Canonical V1 Parquet fixtures

This directory contains a byte-reproducible interoperability corpus:

- `valid-v1.parquet` exercises empty content/tags, variable-length tags,
  Unicode, `u16::MAX` kind, and a `created_at` value above `i64::MAX`.
- each `invalid-*.parquet` file violates one named conformance rule.

Regenerate the corpus from deterministic signed events:

```bash
cargo run -p pensieve-parquet --example generate_parquet_fixtures
```

The strict validator test requires the valid file to pass and every invalid
file to fail.
