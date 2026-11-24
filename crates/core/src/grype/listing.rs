//! Anchore's listing of the latest Grype database (`latest.json`, ADR 0160), read as Grype's `distribution` package
//! reads it: the database's schema, when it was built, the archive's path beside the listing and its SHA-256. It
//! takes only a listing whose schema is the model runnerd's Grype imports, whose path is one file name and whose
//! checksum is a SHA-256. Pure: it neither downloads nor writes.

use serde::Deserialize;

/// The schema model Grype's v6 databases have, and the only one runnerd keeps.
pub(crate) const MODEL: &str = "v6";

/// What the listing says of the latest database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    /// Its schema, such as `v6.1.10`.
    pub(crate) schema: String,
    /// When Anchore built it, in seconds since the epoch.
    pub(crate) built_at: u64,
    /// The archive's file name, beside the listing.
    pub(crate) path: String,
    /// The archive's SHA-256, 64 lowercase hex digits.
    pub(crate) sha256: String,
}

/// `latest.json` as Anchore writes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    status: String,
    schema_version: String,
    built: String,
    path: String,
    checksum: String,
}

/// The listing in `bytes`, or why runnerd does not take it.
pub(crate) fn parse(bytes: &[u8]) -> Result<Listing, String> {
    let document: Document =
        serde_json::from_slice(bytes).map_err(|error| format!("not a listing: {error}"))?;
    // `eol` listings are no longer updated: a database from one goes stale.
    if document.status == "eol" {
        return Err("Anchore no longer updates this schema's databases".to_owned());
    }
    if !is_schema(&document.schema_version) {
        return Err(format!(
            "its schema {} is not {MODEL}",
            document.schema_version
        ));
    }
    let built_at = crate::time::parse_ms(&document.built)
        .ok_or_else(|| format!("its build time {} is not a time", document.built))?
        / 1000;
    if !is_file_name(&document.path) {
        return Err(format!(
            "its archive path {} is not one file name",
            document.path
        ));
    }
    let sha256 = document
        .checksum
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("its checksum {} is not a SHA-256", document.checksum))?
        .to_ascii_lowercase();
    Ok(Listing {
        schema: document.schema_version,
        built_at,
        path: document.path,
        sha256,
    })
}

/// Whether `schema` is `v6.<revision>.<addition>`, as Grype's `schemaver` writes one of the model runnerd keeps.
pub(crate) fn is_schema(schema: &str) -> bool {
    let Some(rest) = schema
        .strip_prefix(MODEL)
        .and_then(|rest| rest.strip_prefix('.'))
    else {
        return false;
    };
    let mut parts = rest.split('.');
    let number = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    number(parts.next()) && number(parts.next()) && parts.next().is_none()
}

/// Whether `path` names one file: what Grype joins to the listing's directory, so no directory, `..`, query or
/// anything a URL would read otherwise.
fn is_file_name(path: &str) -> bool {
    !path.is_empty()
        && path != "."
        && path != ".."
        && path.len() <= 255
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
        && (path.ends_with(".tar.zst") || path.ends_with(".tar.xz") || path.ends_with(".tar.gz"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anchore's listing as served on 2026-11-23.
    const SERVED: &str = r#"{
 "status": "active",
 "schemaVersion": "v6.1.10",
 "built": "2026-11-23T06:28:32Z",
 "path": "vulnerability-db_v6.1.10_2026-11-23T00:24:20Z_1795415312.tar.zst",
 "checksum": "sha256:3d24f57cd3587e2c73f686decc7ad04b213d1f3575bcf1dd847e42df94560f14"
}"#;

    fn with(field: &str, value: &str) -> String {
        let mut document: serde_json::Value = serde_json::from_str(SERVED).unwrap();
        document[field] = serde_json::Value::String(value.to_owned());
        document.to_string()
    }

    #[test]
    fn the_listing_anchore_serves_names_its_archive_and_checksum() {
        assert_eq!(
            parse(SERVED.as_bytes()),
            Ok(Listing {
                schema: "v6.1.10".to_owned(),
                built_at: 1_795_415_312,
                path: "vulnerability-db_v6.1.10_2026-11-23T00:24:20Z_1795415312.tar.zst".to_owned(),
                sha256: "3d24f57cd3587e2c73f686decc7ad04b213d1f3575bcf1dd847e42df94560f14"
                    .to_owned(),
            })
        );
    }

    /// The bug each catches: a listing that would make runnerd fetch outside Anchore's directory, keep a database
    /// no v6 Grype reads, or skip the checksum.
    #[test]
    fn a_listing_runnerd_cannot_trust_or_use_is_refused() {
        for (field, value) in [
            ("path", "../../evil.tar.zst"),
            ("path", "dir/db.tar.zst"),
            ("path", "https://elsewhere/db.tar.zst"),
            ("path", "db.tar.zst?x=1"),
            ("path", "vulnerability.db"),
            ("path", ""),
            ("schemaVersion", "v5.0.2"),
            ("schemaVersion", "v6"),
            ("schemaVersion", "v6.1.x"),
            ("checksum", "sha256:abc"),
            ("checksum", "md5:3d24f57cd3587e2c73f686decc7ad04b"),
            ("checksum", ""),
            ("built", "yesterday"),
            ("status", "eol"),
        ] {
            assert!(
                parse(with(field, value).as_bytes()).is_err(),
                "{field} {value:?} is refused"
            );
        }
        assert!(
            parse(b"<html>").is_err(),
            "a page that is not JSON is refused"
        );
        assert!(parse(b"{}").is_err(), "an empty listing is refused");
    }

    #[test]
    fn a_deprecated_listing_is_still_taken() {
        assert!(
            parse(with("status", "deprecated").as_bytes()).is_ok(),
            "a deprecated schema is still served by Anchore"
        );
    }
}
