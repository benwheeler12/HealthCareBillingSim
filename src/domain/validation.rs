//! Ingest validation: parse → schema → domain. Malformed claims become
//! `Rejected` ledger rows, never silent drops (fault rows 1.1 / 1.2).
//!
//! Pure functions only — no I/O, no time.

use crate::domain::claim::{Claim, ClaimId, PayerClaimDoc, ServiceLine};
use crate::domain::money::Money;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValidationError {
    /// Fault 1.1: the line is not valid JSON at all.
    Parse(String),
    /// Fault 1.2: valid JSON that violates the claim schema; every violation
    /// found is listed.
    Schema(Vec<String>),
}

/// Validate one input line. On failure, also return the claim_id when the
/// document was parseable enough to salvage one, so the rejection can land on
/// a real ledger row.
pub fn validate_line(line: &str) -> Result<Claim, (Option<ClaimId>, ValidationError)> {
    let doc: PayerClaimDoc = match serde_json::from_str(line) {
        Ok(doc) => doc,
        Err(err) => return Err(classify_serde_error(line, err)),
    };
    let violations = semantic_violations(&doc);
    if !violations.is_empty() {
        let id = Some(ClaimId(doc.claim_id.clone()));
        return Err((id, ValidationError::Schema(violations)));
    }
    Ok(to_domain(doc))
}

fn classify_serde_error(
    line: &str,
    err: serde_json::Error,
) -> (Option<ClaimId>, ValidationError) {
    if err.is_syntax() || err.is_eof() {
        return (None, ValidationError::Parse(err.to_string()));
    }
    // Structurally valid JSON with a schema problem (missing field, unknown
    // payer_id, wrong type). Salvage the claim_id if one is present.
    let id = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("claim_id")?.as_str().map(|s| ClaimId(s.to_owned())));
    (id, ValidationError::Schema(vec![err.to_string()]))
}

fn semantic_violations(doc: &PayerClaimDoc) -> Vec<String> {
    let mut violations = Vec::new();
    if !is_npi(&doc.rendering_provider.npi) {
        violations.push(format!(
            "rendering_provider.npi must be 10 digits, got {:?}",
            doc.rendering_provider.npi
        ));
    }
    if !matches!(doc.patient.gender.as_str(), "m" | "f") {
        violations.push(format!("patient.gender must match ^(m|f)$, got {:?}", doc.patient.gender));
    }
    if !is_iso_date(&doc.patient.dob) {
        violations.push(format!("patient.dob must be YYYY-MM-DD, got {:?}", doc.patient.dob));
    }
    for line in &doc.service_lines {
        let id = &line.service_line_id;
        if line.units < 1 {
            violations.push(format!("service_line {id}: units must be >= 1, got {}", line.units));
        }
        if line.units > u32::MAX as i64 {
            violations.push(format!("service_line {id}: units out of range"));
        }
        if line.unit_charge_amount < 0.0 {
            violations.push(format!(
                "service_line {id}: unit_charge_amount must be >= 0, got {}",
                line.unit_charge_amount
            ));
        } else if Money::try_from_dollars(line.unit_charge_amount).is_none() {
            violations.push(format!(
                "service_line {id}: unit_charge_amount {} is not a whole number of cents",
                line.unit_charge_amount
            ));
        }
    }
    violations
}

fn to_domain(doc: PayerClaimDoc) -> Claim {
    let lines = doc
        .service_lines
        .into_iter()
        .map(|l| ServiceLine {
            service_line_id: l.service_line_id,
            procedure_code: l.procedure_code,
            units: l.units as u32,
            // Validated whole-cents above; unwrap is unreachable by construction.
            unit_charge: Money::try_from_dollars(l.unit_charge_amount).expect("validated"),
            do_not_bill: l.do_not_bill,
        })
        .collect();
    Claim {
        claim_id: ClaimId(doc.claim_id),
        payer_id: doc.insurance.payer_id,
        patient_member_id: doc.insurance.patient_member_id,
        provider_npi: doc.rendering_provider.npi,
        organization_name: doc.organization.name,
        lines,
    }
}

fn is_npi(s: &str) -> bool {
    s.len() == 10 && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9].iter().all(|&i| b[i].is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_line() -> String {
        r#"{"claim_id":"c-1","place_of_service_code":11,
            "insurance":{"payer_id":"medicare","patient_member_id":"M123"},
            "patient":{"first_name":"Ada","last_name":"Lovelace","gender":"f","dob":"1990-01-02"},
            "organization":{"name":"Acme Clinic"},
            "rendering_provider":{"first_name":"Grace","last_name":"Hopper","npi":"1234567890"},
            "service_lines":[{"service_line_id":"L1","procedure_code":"99213","units":2,
                "details":"office visit","unit_charge_currency":"USD","unit_charge_amount":123.45}]}"#
            .replace('\n', "")
    }

    #[test]
    fn valid_claim_passes_and_converts_money() {
        let claim = validate_line(&valid_line()).expect("valid");
        assert_eq!(claim.claim_id, ClaimId("c-1".into()));
        assert_eq!(claim.payer_id, crate::domain::PayerId::Medicare);
        assert_eq!(claim.lines[0].billed(), Money::from_cents(24690));
    }

    #[test]
    fn garbage_is_a_parse_error_without_claim_id() {
        let (id, err) = validate_line("{not json").unwrap_err();
        assert_eq!(id, None);
        assert!(matches!(err, ValidationError::Parse(_)));
    }

    #[test]
    fn missing_field_is_a_schema_error_with_salvaged_id() {
        let line = valid_line().replace(r#""place_of_service_code":11,"#, "");
        let (id, err) = validate_line(&line).unwrap_err();
        assert_eq!(id, Some(ClaimId("c-1".into())));
        assert!(matches!(err, ValidationError::Schema(_)));
    }

    #[test]
    fn unknown_payer_is_a_schema_error() {
        let line = valid_line().replace("medicare", "aetna");
        let (id, err) = validate_line(&line).unwrap_err();
        assert_eq!(id, Some(ClaimId("c-1".into())));
        assert!(matches!(err, ValidationError::Schema(_)));
    }

    #[test]
    fn bad_npi_and_negative_charge_list_all_violations() {
        let line = valid_line()
            .replace("1234567890", "12345")
            .replace("123.45", "-1.0");
        let (_, err) = validate_line(&line).unwrap_err();
        let ValidationError::Schema(violations) = err else { panic!("expected schema error") };
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn fractional_cents_are_rejected() {
        let line = valid_line().replace("123.45", "10.005");
        let (_, err) = validate_line(&line).unwrap_err();
        assert!(matches!(err, ValidationError::Schema(_)));
    }
}
