//! Narrow recipient-based admission policy, shared by live and negentropy ingestion.

use sha2::{Digest, Sha256};

/// Immutable, normalized parent admission policy. No worker controls these rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdmissionPolicy {
    recipients: Vec<String>,
}

impl AdmissionPolicy {
    /// Validate and canonicalize keys so ordering and encoding do not alter identity.
    pub fn new(recipients: &[String]) -> Result<Self, String> {
        let mut recipients = recipients
            .iter()
            .map(|key| parse_recipient(key))
            .collect::<Result<Vec<_>, _>>()?;
        recipients.sort();
        recipients.dedup();
        Ok(Self { recipients })
    }

    /// Versioned digest of the exact rule semantics and canonical configuration.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new_with_prefix(b"pensieve-giftwrap-recipient-policy-v1\0");
        for key in &self.recipients {
            hash.update(key.as_bytes());
        }
        hash.finalize().into()
    }

    /// Parent-only decision using the same predicate as live admission.
    pub fn blocks(&self, event: &nostr_sdk::Event) -> bool {
        is_blocked(event, &self.recipients)
    }
}

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
        assert_eq!(
            AdmissionPolicy::new(&[KEY.to_string(), NPUB.to_string()])
                .unwrap()
                .fingerprint(),
            AdmissionPolicy::new(&[NPUB.to_string()])
                .unwrap()
                .fingerprint()
        );
        assert_ne!(
            AdmissionPolicy::default().fingerprint(),
            AdmissionPolicy::new(&[KEY.to_string()])
                .unwrap()
                .fingerprint()
        );
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
