//! llms.txt and llms-full.txt endpoints for LLM agent discoverability.
//!
//! Follows the llms.txt convention (https://llmstxt.org/) to help AI agents
//! understand how to interact with this service.

use axum::response::IntoResponse;

/// Serve llms.txt — concise description for LLM agents.
pub async fn llms_txt() -> impl IntoResponse {
    ([("content-type", "text/plain; charset=utf-8")], LLMS_TXT)
}

/// Serve llms-full.txt — detailed description with examples.
pub async fn llms_full_txt() -> impl IntoResponse {
    (
        [("content-type", "text/plain; charset=utf-8")],
        LLMS_FULL_TXT,
    )
}

const LLMS_TXT: &str = r#"# Nostr Preview

> Static HTML preview pages for Nostr events, powered by the Pensieve archive.

This service renders preview pages for any Nostr event, profile, or entity.
It accepts NIP-19 identifiers (npub, note, nevent, nprofile, naddr) or raw
64-character hex event IDs and pubkeys.

## JSON API

Append `.json` to any identifier URL to get the raw event data as JSON:

    GET /{identifier}.json

Returns event content, tags, author metadata, and engagement stats
(reactions, replies, reposts, zap amounts).

## Supported Event Types

- Profiles (kind 0) — via npub or nprofile
- Text notes (kind 1) — via note or nevent
- Long-form articles (kind 30023) — via naddr or event ID
- Videos (kind 34235/34236) — via event ID
- Reposts (kind 6/16) — via event ID
- Any other kind — generic renderer

## Links

- Source code: https://github.com/andotherstuff/pensieve
- Organization: https://andotherstuff.org
"#;

const LLMS_FULL_TXT: &str = r#"# Nostr Preview — Full Documentation

> Static HTML preview pages for Nostr events, powered by the Pensieve archive.

## Overview

This service provides fast, static HTML preview pages for Nostr events.
It is designed for link embeds (Open Graph / Twitter Cards), direct viewing,
and programmatic access by LLM agents.

## URL Patterns

### HTML Preview
    GET /{identifier}

### JSON API
    GET /{identifier}.json

### Identifier Formats

- `npub1...` — Nostr public key (bech32)
- `nprofile1...` — Profile with relay hints (bech32)
- `note1...` — Event ID (bech32)
- `nevent1...` — Event with relay hints (bech32)
- `naddr1...` — Replaceable event coordinate (bech32)
- 64-character hex — Raw event ID or pubkey

## JSON Response Format

All JSON responses follow this structure:

```json
{
  "event": {
    "id": "hex_event_id",
    "pubkey": "hex_pubkey",
    "created_at": 1234567890,
    "kind": 1,
    "content": "...",
    "tags": [["e", "..."], ["p", "..."]]
  },
  "author_metadata": {
    "name": "...",
    "display_name": "...",
    "about": "...",
    "picture": "https://...",
    "banner": "https://...",
    "nip05": "user@domain.com",
    "lud16": "user@wallet.com",
    "website": "https://..."
  },
  "mentions_metadata": {
    "hex_pubkey_1": { "name": "...", "display_name": "...", ... },
    "hex_pubkey_2": { "name": "...", "display_name": "...", ... }
  },
  "engagement": {
    "reactions": 42,
    "comments": 10,
    "reposts": 5,
    "zap_msats": 100000,
    "zap_count": 3
  }
}
```

- `event` — The raw Nostr event with all standard fields
- `author_metadata` — Parsed kind 0 profile of the event author (null if no profile found)
- `mentions_metadata` — Map of pubkeys mentioned in tags to their kind 0 profiles
- `engagement` — Reaction, comment, repost, and zap counts (null for profiles)

For profiles (npub/nprofile), `engagement` is null and the event is a synthetic kind 0.

## Nostr Protocol Reference

For detailed information about Nostr event kinds, tags, and protocol semantics,
use the Nostr MCP server: https://github.com/nostr-protocol/nostr-mcp

This MCP server provides tools for reading NIPs, event kind documentation,
and tag specifications — useful for understanding event content and structure.

## Notes

- Events are sourced from the Pensieve Nostr archive (ClickHouse-backed)
- Not all events may be available — only those ingested by Pensieve
- Engagement counts are approximate (based on ingested data)
- Zap amounts are in millisatoshis (divide by 1000 for sats)
- HTML pages include Open Graph and Twitter Card meta tags
- All dynamic content is HTML-escaped for XSS protection

## Links

- Pensieve source code: https://github.com/andotherstuff/pensieve
- And Other Stuff (organization): https://andotherstuff.org
- Nostr MCP server (protocol reference): https://github.com/nostr-protocol/nostr-mcp
"#;
