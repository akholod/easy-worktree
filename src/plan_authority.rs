//! Trust boundary for an already-read, user-confirmed operation-plan file.

use crate::lifecycle::OperationPlan;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use std::fmt;

pub const MAX_PLAN_BYTES: usize = 16 * 1024 * 1024;

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let difference = left
        .iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right));
    difference == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAuthorityError {
    DigestInvalid,
    DigestMismatch,
    TooLarge,
    JsonInvalid,
    NonCanonical,
    NotExecutable,
}

impl PlanAuthorityError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::DigestInvalid => "digest_invalid",
            Self::DigestMismatch => "digest_mismatch",
            Self::TooLarge => "plan_file_too_large",
            Self::JsonInvalid => "json_invalid",
            Self::NonCanonical => "plan_noncanonical",
            Self::NotExecutable => "plan_not_executable",
        }
    }
}

impl fmt::Display for PlanAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for PlanAuthorityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedPlan {
    plan: OperationPlan,
    raw_digest: String,
}

impl ConfirmedPlan {
    pub fn plan(&self) -> &OperationPlan {
        &self.plan
    }
    pub fn into_plan(self) -> OperationPlan {
        self.plan
    }
    pub fn raw_digest(&self) -> &str {
        &self.raw_digest
    }
}

/// Confirms and decodes a bare `OperationPlan`. The caller owns the input bytes;
/// this function retains only the decoded plan and its digest.
pub fn confirm_plan(
    raw: &[u8],
    expected_digest: &str,
) -> Result<ConfirmedPlan, PlanAuthorityError> {
    if raw.len() > MAX_PLAN_BYTES {
        return Err(PlanAuthorityError::TooLarge);
    }
    if expected_digest.len() != 64
        || !expected_digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(PlanAuthorityError::DigestInvalid);
    }
    let digest = Sha256::digest(raw);
    let actual = lowercase_hex(&digest);
    if !constant_time_equal(actual.as_bytes(), expected_digest.as_bytes()) {
        return Err(PlanAuthorityError::DigestMismatch);
    }

    let text = std::str::from_utf8(raw).map_err(|_| PlanAuthorityError::JsonInvalid)?;
    let mut de = serde_json::Deserializer::from_str(text);
    let value = StrictValueSeed
        .deserialize(&mut de)
        .map_err(|_| PlanAuthorityError::JsonInvalid)?;
    de.end().map_err(|_| PlanAuthorityError::JsonInvalid)?;
    if !value.is_object() {
        return Err(PlanAuthorityError::NonCanonical);
    }
    let plan: OperationPlan =
        serde_json::from_value(value.clone()).map_err(|_| PlanAuthorityError::NonCanonical)?;
    if serde_json::to_value(&plan).map_err(|_| PlanAuthorityError::NonCanonical)? != value {
        return Err(PlanAuthorityError::NonCanonical);
    }
    plan.validate_persisted()
        .map_err(|_| PlanAuthorityError::NonCanonical)?;
    plan.validate_executable_plan()
        .map_err(|_| PlanAuthorityError::NotExecutable)?;
    Ok(ConfirmedPlan {
        plan,
        raw_digest: actual,
    })
}

struct StrictValueSeed;
impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = serde_json::Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(StrictValueVisitor)
    }
}
struct StrictValueVisitor;
impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = serde_json::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("strict JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(v)
            .map(Into::into)
            .ok_or_else(|| serde::de::Error::custom("invalid number"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(v.into())
    }
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(v.into())
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = a.next_element_seed(StrictValueSeed)? {
            out.push(v);
        }
        Ok(out.into())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = a.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate object key"));
            }
            let value = a.next_value_seed(StrictValueSeed)?;
            out.insert(key, value);
        }
        Ok(out.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(raw: &[u8]) -> String {
        lowercase_hex(&Sha256::digest(raw))
    }

    fn error_code(raw: &[u8], expected: &str) -> &'static str {
        confirm_plan(raw, expected).unwrap_err().code()
    }

    #[test]
    fn digest_is_checked_before_json() {
        let raw = b"not json";
        assert_eq!(
            confirm_plan(raw, &"0".repeat(64)),
            Err(PlanAuthorityError::DigestMismatch)
        );
        assert_eq!(
            confirm_plan(raw, "UPPER"),
            Err(PlanAuthorityError::DigestInvalid)
        );
    }

    #[test]
    fn digest_mutations_and_lowercase_contract() {
        let plan = crate::planner::tests::authority_fixture();
        let raw = serde_json::to_vec(&plan).unwrap();
        let expected = digest(&raw);
        let confirmed = confirm_plan(&raw, &expected).unwrap();
        assert_eq!(confirmed.raw_digest(), expected);
        assert_eq!(confirmed.plan(), &plan);
        let mut changed = raw.clone();
        changed[0] = if changed[0] == b'{' { b' ' } else { b'{' };
        assert_eq!(error_code(&changed, &expected), "digest_mismatch");
        assert_eq!(
            error_code(
                &serde_json::to_string_pretty(&plan).unwrap().into_bytes(),
                &expected
            ),
            "digest_mismatch"
        );
        for invalid in [
            expected.to_uppercase(),
            format!("0x{expected}"),
            format!(" {expected}"),
            expected[..63].to_owned(),
            format!("{expected}0"),
            format!("{}-", &expected[..10]),
        ] {
            assert_eq!(error_code(&raw, &invalid), "digest_invalid");
        }
        let alternate = serde_json::to_string_pretty(&plan).unwrap().into_bytes();
        assert_eq!(
            confirm_plan(&alternate, &digest(&alternate))
                .unwrap()
                .plan(),
            &plan
        );
        let escaped = String::from_utf8(raw.clone())
            .unwrap()
            .replace('/', "\\/")
            .into_bytes();
        assert_eq!(
            confirm_plan(&escaped, &digest(&escaped)).unwrap().plan(),
            &plan
        );
    }

    #[test]
    fn size_limit_precedes_digest_and_json() {
        assert_eq!(MAX_PLAN_BYTES, 16 * 1024 * 1024);
        let raw = vec![0xff; MAX_PLAN_BYTES + 1];
        assert_eq!(
            confirm_plan(&raw, "not-a-digest"),
            Err(PlanAuthorityError::TooLarge)
        );
        assert_eq!(
            confirm_plan(&vec![b'{'; MAX_PLAN_BYTES], &"0".repeat(64)),
            Err(PlanAuthorityError::DigestMismatch)
        );
    }

    #[test]
    fn malformed_and_non_object_json_are_rejected_without_leaking_bytes() {
        let cases: &[&[u8]] = &[
            b"{",
            b"{} {}",
            b"[1,2,3]",
            b"null",
            b"{\"schema_version\":3}",
            b"{\"secret\":\"marker\",\"secret\":\"marker\"}",
            b"{\"outer\":{\"secret\":1,\"secret\":2}}",
            b"{\"unknown_field\":true}",
        ];
        for raw in cases {
            let expected = digest(raw);
            let error = confirm_plan(raw, &expected).unwrap_err();
            assert!(!format!("{error:?}").contains("secret"));
            assert!(!error.to_string().contains("secret"));
        }
        assert_eq!(
            error_code(&[0xff, 0xfe], &digest(&[0xff, 0xfe])),
            "json_invalid"
        );
        assert_eq!(error_code(b"[{}]", &digest(b"[{}]")), "plan_noncanonical");
        assert_eq!(error_code(b"1", &digest(b"1")), "plan_noncanonical");
        assert_eq!(error_code(b"null", &digest(b"null")), "plan_noncanonical");
    }

    #[test]
    fn strict_duplicate_detection_is_recursive() {
        let raw = br#"{"a":{"b":{"c":1,"c":2}}}"#;
        assert_eq!(error_code(raw, &digest(raw)), "json_invalid");
    }

    #[test]
    fn envelope_and_omitted_default_shapes_are_noncanonical() {
        let plan = crate::planner::tests::authority_fixture();
        let canonical = serde_json::to_value(&plan).unwrap();
        let envelope = serde_json::json!({"plan": canonical});
        let raw = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(error_code(&raw, &digest(&raw)), "plan_noncanonical");

        let mut omitted = serde_json::to_value(&plan).unwrap();
        omitted["intent"]["Create"]
            .as_object_mut()
            .unwrap()
            .remove("task_contracts");
        let raw = serde_json::to_vec(&omitted).unwrap();
        assert_eq!(error_code(&raw, &digest(&raw)), "plan_noncanonical");
    }

    #[test]
    fn persisted_shape_can_still_be_rejected_as_not_executable() {
        let plan = crate::planner::tests::authority_fixture();
        let mut wire = serde_json::to_value(&plan).unwrap();
        wire["plan_schema_version"] = serde_json::json!(1);
        let raw = serde_json::to_vec(&wire).unwrap();
        assert_eq!(error_code(&raw, &digest(&raw)), "plan_not_executable");
    }
}
