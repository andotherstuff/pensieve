//! Narrow recipient-based admission policy, shared by live and negentropy ingestion.

/// Parse a configured hex or npub key once, rejecting invalid configuration.
pub fn parse_recipient(value: &str) -> Result<String, String> {
    nostr_sdk::PublicKey::parse(value)
        .map(|key| key.to_hex())
        .map_err(|_| "expected a valid recipient public key (hex or npub)".to_string())
}

/// Whether this event should be dropped before dedupe or archive admission.
pub fn is_blocked(event: &nostr_sdk::Event, recipients: &[String]) -> bool {
    matches_tags(
        event.kind.as_u16(),
        event.tags.iter().map(|tag| tag.as_slice()),
        recipients,
    )
}

fn matches_tags<'a, I>(kind: u16, tags: I, recipients: &[String]) -> bool
where
    I: IntoIterator<Item = &'a [String]>,
{
    kind == 1059
        && !recipients.is_empty()
        && tags.into_iter().any(|tag| {
            tag.first().is_some_and(|name| name == "p")
                && tag.get(1).is_some_and(|key| {
                    recipients
                        .iter()
                        .any(|blocked| key.eq_ignore_ascii_case(blocked))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "d0182d1b03c946beeb5d4b6742216c021f38322e8f089bf63722cdddc1f819aa";
    const NPUB: &str = "npub16qvz6xcre9rta66afdn5ygtvqg0nsv3w3uyfha3hytxams0crx4qpz97ax";

    fn blocked(kind: u16, tags: &[Vec<String>], recipients: &[String]) -> bool {
        matches_tags(kind, tags.iter().map(Vec::as_slice), recipients)
    }

    #[test]
    fn validates_configuration() {
        assert_eq!(parse_recipient(NPUB).unwrap(), KEY);
        assert_eq!(parse_recipient(KEY).unwrap(), KEY);
        assert!(parse_recipient("invalid").is_err());
    }

    #[test]
    fn matches_only_giftwrap_recipient_tags() {
        let recipients = vec![KEY.to_string()];
        let tags = vec![vec!["p".to_string(), KEY.to_string()]];
        assert!(blocked(1059, &tags, &recipients));
        assert!(!blocked(1, &tags, &recipients));
        assert!(!blocked(1059, &tags, &[]));
        for name in ["e", "P", "recipient"] {
            assert!(!blocked(
                1059,
                &[vec![name.to_string(), KEY.to_string()]],
                &recipients
            ));
        }
        assert!(!blocked(
            1059,
            &[vec!["p".to_string(), "00".repeat(32)]],
            &recipients
        ));
        assert!(!blocked(
            1059,
            &[vec![], vec!["p".to_string()]],
            &recipients
        ));
    }

    #[test]
    fn checks_every_tag_and_accepts_optional_relay_hint() {
        let tags = vec![
            vec!["p".to_string(), "00".repeat(32)],
            vec![
                "p".to_string(),
                KEY.to_uppercase(),
                "wss://relay.ditto.pub".to_string(),
            ],
        ];
        assert!(blocked(1059, &tags, &[KEY.to_string()]));
    }
}
