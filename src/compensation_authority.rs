//! The deliberately narrow authority boundary for compensation.
use crate::compensation::{CompensationProposalV1, ProposalId, Sha256Digest};
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use std::{fmt, path::Path};

pub const MAX_PROPOSAL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalFileError {
    NotRegular,
    Io,
    TooLarge,
    PlatformUnsupported,
    InvalidConfirmation,
    InvalidUtf8,
    InvalidJson,
    DuplicateKeys,
    NonCanonical,
    InvalidProposal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedProposal {
    proposal: CompensationProposalV1,
    raw_sha256: Sha256Digest,
    raw: Vec<u8>,
}

impl LoadedProposal {
    pub fn proposal(&self) -> &CompensationProposalV1 {
        &self.proposal
    }
    pub fn raw_sha256(&self) -> &Sha256Digest {
        &self.raw_sha256
    }
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }
}

pub fn load(path: &Path, confirmation: &str) -> Result<LoadedProposal, ProposalFileError> {
    let raw = crate::system::read_proposal_file(path)?;
    load_bytes(raw, confirmation)
}

pub fn load_bytes(raw: Vec<u8>, confirmation: &str) -> Result<LoadedProposal, ProposalFileError> {
    if raw.len() > MAX_PROPOSAL_BYTES {
        return Err(ProposalFileError::TooLarge);
    }
    if confirmation.len() != 64
        || !confirmation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ProposalFileError::InvalidConfirmation);
    }
    let digest = format!("{:x}", Sha256::digest(&raw));
    let equal = digest
        .as_bytes()
        .iter()
        .zip(confirmation.as_bytes())
        .fold(0u8, |v, (a, b)| v | (a ^ b))
        == 0;
    if !equal {
        return Err(ProposalFileError::InvalidConfirmation);
    }
    let text = std::str::from_utf8(&raw).map_err(|_| ProposalFileError::InvalidUtf8)?;
    if has_duplicate_keys(&raw) {
        return Err(ProposalFileError::DuplicateKeys);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| ProposalFileError::InvalidJson)?;
    if !value.is_object() {
        return Err(ProposalFileError::InvalidJson);
    }
    let proposal: CompensationProposalV1 =
        serde_json::from_str(text).map_err(|_| ProposalFileError::InvalidProposal)?;
    let roundtrip = serde_json::to_value(&proposal).map_err(|_| ProposalFileError::NonCanonical)?;
    if roundtrip != value {
        return Err(ProposalFileError::NonCanonical);
    }
    proposal
        .validate()
        .map_err(|_| ProposalFileError::InvalidProposal)?;
    Ok(LoadedProposal {
        proposal,
        raw_sha256: Sha256Digest::new(digest).expect("sha256"),
        raw,
    })
}

pub(crate) fn has_duplicate_keys(raw: &[u8]) -> bool {
    struct Seed;
    impl<'de> DeserializeSeed<'de> for Seed {
        type Value = bool;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Seed {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("json")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<bool, A::Error> {
            let mut keys = std::collections::BTreeSet::new();
            let mut bad = false;
            while let Some(k) = a.next_key::<String>()? {
                if !keys.insert(k) {
                    bad = true
                };
                if a.next_value_seed(Seed)? {
                    bad = true
                }
            }
            Ok(bad)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<bool, A::Error> {
            let mut bad = false;
            while let Some(v) = a.next_element_seed(Seed)? {
                bad |= v
            }
            Ok(bad)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_borrowed_str<E: serde::de::Error>(self, _: &'de str) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
    }
    serde_json::Deserializer::from_slice(raw)
        .deserialize_any(Seed)
        .unwrap_or(true)
}

pub fn proposal_filename(id: &ProposalId) -> String {
    format!("{id}.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};
    use tempfile::tempdir;

    fn confirmation(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    fn error(bytes: Vec<u8>) -> ProposalFileError {
        load_bytes(bytes.clone(), &confirmation(&bytes)).unwrap_err()
    }

    #[test]
    fn confirmation_syntax_is_checked_before_content() {
        assert_eq!(
            load_bytes(vec![], "A"),
            Err(ProposalFileError::InvalidConfirmation)
        );
    }
    #[test]
    fn exact_limit_and_limit_plus_one_are_bounded() {
        let mut exact = Vec::with_capacity(MAX_PROPOSAL_BYTES);
        exact.extend_from_slice(br#"{"x":"#);
        exact.extend(std::iter::repeat_n(b'x', MAX_PROPOSAL_BYTES - 8));
        exact.extend_from_slice(br#""}"#);
        assert_ne!(
            load_bytes(exact, &"0".repeat(64)),
            Err(ProposalFileError::TooLarge)
        );
        let too_large = vec![b'x'; MAX_PROPOSAL_BYTES + 1];
        assert_eq!(
            load_bytes(too_large, &"0".repeat(64)),
            Err(ProposalFileError::TooLarge)
        );
    }
    #[test]
    fn digest_is_byte_sensitive_and_lowercase_only() {
        let bytes = b"{}".to_vec();
        let upper = confirmation(&bytes).to_uppercase();
        assert_eq!(
            load_bytes(bytes.clone(), &upper),
            Err(ProposalFileError::InvalidConfirmation)
        );
        let mut changed = bytes;
        changed.push(b' ');
        assert_eq!(
            load_bytes(changed, &confirmation(b"{}")),
            Err(ProposalFileError::InvalidConfirmation)
        );
    }
    #[test]
    fn utf8_object_envelope_and_trailing_are_rejected() {
        assert_eq!(error(vec![0xff]), ProposalFileError::InvalidUtf8);
        assert_eq!(error(b"[]".to_vec()), ProposalFileError::InvalidJson);
        assert_eq!(
            error(b"{\"proposal\":{}}".to_vec()),
            ProposalFileError::InvalidProposal
        );
        assert_eq!(error(b"{} {}".to_vec()), ProposalFileError::InvalidJson);
    }
    #[test]
    fn recursive_duplicates_are_rejected_before_decode() {
        assert_eq!(
            error(br#"{"a":{"x":1,"x":2}}"#.to_vec()),
            ProposalFileError::DuplicateKeys
        );
    }
    #[test]
    fn unknown_and_noncanonical_shapes_fail_closed() {
        assert_eq!(
            error(br#"{"unknown":1}"#.to_vec()),
            ProposalFileError::InvalidProposal
        );
        assert_eq!(
            error(br#"{"x":1.0}"#.to_vec()),
            ProposalFileError::InvalidProposal
        );
    }
    #[test]
    fn raw_reader_rejects_empty_missing_directory_and_symlink() {
        let dir = tempdir().unwrap();
        assert_eq!(
            crate::system::read_proposal_file(Path::new("")),
            Err(ProposalFileError::NotRegular)
        );
        assert_eq!(
            crate::system::read_proposal_file(&dir.path().join("missing")),
            Err(ProposalFileError::Io)
        );
        assert_eq!(
            crate::system::read_proposal_file(dir.path()),
            Err(ProposalFileError::NotRegular)
        );
        #[cfg(unix)]
        {
            let target = dir.path().join("target");
            fs::write(&target, b"{}").unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(target, link.clone()).unwrap();
            assert_eq!(
                crate::system::read_proposal_file(&link),
                Err(ProposalFileError::NotRegular)
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn held_reader_reads_original_after_path_replacement() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("proposal");
        let replacement = dir.path().join("replacement");
        fs::write(&path, b"original").unwrap();
        fs::write(&replacement, b"replacement").unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(&path)
            .unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&replacement, &path).unwrap();
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take((MAX_PROPOSAL_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"original");
    }
    #[cfg(unix)]
    #[test]
    fn fifo_is_refused_without_blocking() {
        let dir = tempdir().unwrap();
        let fifo = dir.path().join("proposal.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            crate::system::read_proposal_file(&fifo),
            Err(ProposalFileError::NotRegular)
        );
    }
}
