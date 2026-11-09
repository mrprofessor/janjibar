//! Decoding a release payload: base64, then gzip, then JSON.
//!
//! Helm marshals the release to JSON, gzips it, base64-encodes it and stores
//! the text under the `release` key. In a Secret the API base64-encodes that
//! text once more on the wire; k8s-openapi undoes that layer for us. A
//! `ConfigMap` holds the text as is.

use std::io::Read as _;
use std::sync::Arc;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::GzDecoder;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::jiff::Timestamp;
use model::{HelmChart, HelmReleaseDetails, HelmStatus};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

/// The data key that holds the payload, for both storage kinds.
pub const DATA_KEY: &str = "release";

/// gzip's magic bytes plus the deflate method. Helm checks these three
/// bytes and reads anything else as plain JSON, which very early Helm 3
/// releases stored.
const GZIP_MAGIC: [u8; 3] = [0x1f, 0x8b, 0x08];

/// The largest payload we unpack. Stored payloads are under 1 MiB of gzip
/// and rarely unpack past a few MiB. The cap stops a corrupt or hostile
/// record from filling memory.
const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;

/// Go's zero `time.Time`. Helm 3 writes it for times it never set, such as
/// `deleted` on a live release. Helm 4 leaves them out.
const GO_ZERO_TIME_SECONDS: i64 = -62_135_596_800;

/// Why a payload could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The object has no `release` data key.
    #[error("the object has no `release` data")]
    Missing,
    /// The payload is not base64.
    #[error("the payload is not valid base64")]
    Base64(#[source] base64::DecodeError),
    /// The payload starts like gzip but does not unpack.
    #[error("the payload is not valid gzip")]
    Gzip(#[source] std::io::Error),
    /// The payload unpacks past [`MAX_JSON_BYTES`].
    #[error("the payload unpacks to more than {MAX_JSON_BYTES} bytes")]
    TooLarge,
    /// The JSON does not have the shape of a Helm release.
    #[error("the payload is not a Helm release")]
    Json(#[source] serde_json::Error),
    /// The user values could not be written as YAML.
    #[error("the values could not be shown as YAML")]
    Values(#[source] serde_saphyr::SerializeError),
}

/// Decodes a Secret written by Helm's secrets driver.
pub fn decode_secret(secret: &Secret) -> Result<HelmReleaseDetails, DecodeError> {
    let data = secret
        .data
        .as_ref()
        .and_then(|data| data.get(DATA_KEY))
        .ok_or(DecodeError::Missing)?;
    decode(&data.0)
}

/// Decodes a `ConfigMap` written by Helm's configmap driver.
pub fn decode_config_map(config_map: &ConfigMap) -> Result<HelmReleaseDetails, DecodeError> {
    let data = config_map
        .data
        .as_ref()
        .and_then(|data| data.get(DATA_KEY))
        .ok_or(DecodeError::Missing)?;
    decode(data.as_bytes())
}

/// Decodes the payload text exactly as Helm wrote it: base64 of gzip of JSON.
///
/// Fields the payload leaves out come back empty: an empty name, revision
/// 0, no times.
pub fn decode(encoded: &[u8]) -> Result<HelmReleaseDetails, DecodeError> {
    // Go's decoder skips line breaks; ours does not.
    let compact: Vec<u8> = encoded
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let packed = STANDARD.decode(compact).map_err(DecodeError::Base64)?;
    let json = unpack(packed, MAX_JSON_BYTES)?;
    let release: Release = serde_json::from_slice(&json).map_err(DecodeError::Json)?;
    release.into_details()
}

/// Gunzips `bytes` if they are gzip, refusing more than `limit` bytes out.
fn unpack(bytes: Vec<u8>, limit: u64) -> Result<Vec<u8>, DecodeError> {
    if !bytes.starts_with(&GZIP_MAGIC) {
        return Ok(bytes);
    }
    let mut json = Vec::new();
    GzDecoder::new(bytes.as_slice())
        .take(limit.saturating_add(1))
        .read_to_end(&mut json)
        .map_err(DecodeError::Gzip)?;
    if json.len() as u64 > limit {
        return Err(DecodeError::TooLarge);
    }
    Ok(json)
}

/// The parts of Helm's `release.Release` JSON we show. Serde skips the rest
/// (chart templates and files, hooks, resources), so they cost a parse but
/// no memory.
#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    info: Option<Info>,
    #[serde(default)]
    chart: Option<Chart>,
    #[serde(default)]
    config: Option<Map<String, Value>>,
    #[serde(default)]
    manifest: String,
}

#[derive(Deserialize, Default)]
struct Info {
    #[serde(default)]
    first_deployed: Option<String>,
    #[serde(default)]
    last_deployed: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    notes: String,
}

#[derive(Deserialize)]
struct Chart {
    #[serde(default)]
    metadata: Option<ChartMetadata>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ChartMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    app_version: String,
}

impl Release {
    fn into_details(self) -> Result<HelmReleaseDetails, DecodeError> {
        let info = self.info.unwrap_or_default();
        let chart = self
            .chart
            .and_then(|chart| chart.metadata)
            .unwrap_or_default();
        Ok(HelmReleaseDetails {
            namespace: self.namespace.into(),
            name: self.name.into(),
            revision: self.version,
            status: HelmStatus::parse(&info.status),
            chart: HelmChart {
                name: chart.name.into(),
                version: chart.version.into(),
                app_version: chart.app_version.into(),
            },
            first_deployed: instant(info.first_deployed.as_deref()),
            updated: instant(info.last_deployed.as_deref()),
            description: info.description.into(),
            notes: info.notes.into(),
            values: values_yaml(self.config.as_ref())?,
            manifest: self.manifest.into(),
        })
    }
}

/// Parses a Go RFC 3339 time. Go's zero time means "never set".
fn instant(text: Option<&str>) -> Option<SystemTime> {
    let text = text?;
    match text.parse::<Timestamp>() {
        Ok(t) if t.as_second() == GO_ZERO_TIME_SECONDS => None,
        Ok(t) => Some(SystemTime::from(t)),
        Err(err) => {
            // A bad time should not hide the rest of the release.
            tracing::debug!("ignoring release time {text:?}: {err}");
            None
        }
    }
}

/// User values as YAML, like `helm get values`.
fn values_yaml(config: Option<&Map<String, Value>>) -> Result<Arc<str>, DecodeError> {
    match config {
        Some(values) if !values.is_empty() => serde_saphyr::to_string(&SortedMap(values))
            .map(Arc::from)
            .map_err(DecodeError::Values),
        _ => Ok(Arc::from("")),
    }
}

/// Serializes a JSON object with its keys sorted at every level, the way
/// Helm prints values. `serde_json` keeps insertion order instead whenever
/// any crate in the build turns on its `preserve_order` feature.
struct SortedMap<'a>(&'a Map<String, Value>);

struct Sorted<'a>(&'a Value);

impl Serialize for SortedMap<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut entries: Vec<(&String, &Value)> = self.0.iter().collect();
        entries.sort_unstable_by_key(|(key, _)| *key);
        let mut map = serializer.serialize_map(Some(entries.len()))?;
        for (key, value) in entries {
            map.serialize_entry(key, &Sorted(value))?;
        }
        map.end()
    }
}

impl Serialize for Sorted<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(map) => SortedMap(map).serialize(serializer),
            Value::Array(items) => serializer.collect_seq(items.iter().map(Sorted)),
            other => other.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::time::Duration;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;

    use super::*;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// Encodes a release the way Helm's storage driver does.
    fn helm_encode(release: &serde_json::Value) -> String {
        STANDARD.encode(gzip(release.to_string().as_bytes()))
    }

    /// A Secret as the API server sends it: Helm's text, base64 again.
    fn secret_with(payload: &[u8]) -> Secret {
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "sh.helm.release.v1.web.v2", "namespace": "shop" },
            "type": "helm.sh/release.v1",
            "data": { "release": STANDARD.encode(payload) },
        }))
        .unwrap()
    }

    fn at(rfc3339: &str) -> SystemTime {
        SystemTime::from(rfc3339.parse::<Timestamp>().unwrap())
    }

    fn sample_release() -> serde_json::Value {
        json!({
            "name": "web",
            "namespace": "shop",
            "version": 2,
            "info": {
                "first_deployed": "2026-10-01T09:00:00.123456+13:00",
                "last_deployed": "2026-11-02T14:30:05.5+13:00",
                "deleted": "0001-01-01T00:00:00Z",
                "description": "Upgrade complete",
                "status": "deployed",
                "notes": "Visit http://web.shop.svc\n",
            },
            "chart": {
                "metadata": {
                    "name": "web",
                    "version": "1.4.0",
                    "appVersion": "2.31.0",
                    "apiVersion": "v2",
                },
                "templates": [{ "name": "templates/cm.yaml", "data": "a2luZDogQ29uZmlnTWFw" }],
                "values": { "replicas": 1 },
            },
            "config": { "replicas": 3, "image": { "tag": "2.31.0" } },
            "manifest": "---\n# Source: web/templates/cm.yaml\nkind: ConfigMap\n",
        })
    }

    #[test]
    fn decodes_a_release_built_in_the_test() {
        let secret = secret_with(helm_encode(&sample_release()).as_bytes());
        let details = decode_secret(&secret).unwrap();

        assert_eq!(&*details.name, "web");
        assert_eq!(&*details.namespace, "shop");
        assert_eq!(details.revision, 2);
        assert_eq!(details.status, HelmStatus::Deployed);
        assert_eq!(&*details.chart.name, "web");
        assert_eq!(&*details.chart.version, "1.4.0");
        assert_eq!(&*details.chart.app_version, "2.31.0");
        assert_eq!(details.chart.to_string(), "web-1.4.0");
        assert_eq!(
            details.first_deployed,
            Some(at("2026-09-30T20:00:00.123456Z"))
        );
        assert_eq!(details.updated, Some(at("2026-11-02T01:30:05.5Z")));
        assert_eq!(&*details.description, "Upgrade complete");
        assert_eq!(&*details.notes, "Visit http://web.shop.svc\n");
        assert_eq!(&*details.values, "image:\n  tag: \"2.31.0\"\nreplicas: 3\n");
        assert!(details.manifest.contains("kind: ConfigMap"));
    }

    #[test]
    fn the_api_base64_layer_is_already_removed_by_k8s_openapi() {
        // Feeding the wire value straight to `decode` would be base64 of
        // base64; the Secret type must hand us Helm's text.
        let encoded = helm_encode(&sample_release());
        let secret = secret_with(encoded.as_bytes());
        let data = &secret.data.as_ref().unwrap()[DATA_KEY];
        assert_eq!(data.0, encoded.as_bytes());
    }

    #[test]
    fn decodes_a_helm3_secret_fixture() {
        let secret: Secret =
            serde_json::from_str(include_str!("../tests/fixtures/helm3-secret.json")).unwrap();
        let details = decode_secret(&secret).unwrap();

        assert_eq!(&*details.name, "ledger");
        assert_eq!(&*details.namespace, "payments");
        assert_eq!(details.revision, 3);
        assert_eq!(details.status, HelmStatus::Deployed);
        assert_eq!(details.chart.to_string(), "ledger-0.7.2");
        assert_eq!(&*details.chart.app_version, "1.19.4");
        assert_eq!(
            details.first_deployed,
            Some(at("2026-09-14T08:12:44.918273Z"))
        );
        assert_eq!(details.updated, Some(at("2026-11-03T21:04:50.004188Z")));
        assert_eq!(&*details.description, "Upgrade complete");
        assert!(details.notes.starts_with("The ledger is running."));
        assert_eq!(
            &*details.values,
            "image:\n  tag: \"1.19.4\"\npersistence:\n  size: 20Gi\nreplicaCount: 2\n"
        );
        assert!(details.manifest.contains("kind: StatefulSet"));
        assert!(
            !details.manifest.contains("helm.sh/hook"),
            "hooks live outside the manifest"
        );
    }

    #[test]
    fn decodes_a_helm4_config_map_fixture() {
        let config_map: ConfigMap =
            serde_json::from_str(include_str!("../tests/fixtures/helm4-configmap.json")).unwrap();
        let details = decode_config_map(&config_map).unwrap();

        assert_eq!(&*details.name, "edge");
        assert_eq!(&*details.namespace, "ingress");
        assert_eq!(details.revision, 2);
        assert_eq!(details.status, HelmStatus::Failed);
        assert_eq!(details.chart.to_string(), "edge-proxy-2.0.0");
        assert_eq!(&*details.chart.app_version, "");
        assert_eq!(details.first_deployed, Some(at("2026-11-08T23:40:02.51Z")));
        assert_eq!(details.updated, Some(at("2026-11-09T00:02:17.7741Z")));
        assert!(details.description.starts_with("Upgrade \"edge\" failed"));
        assert_eq!(&*details.notes, "");
        assert_eq!(&*details.values, "");
        assert!(details.manifest.contains("kind: Deployment"));
    }

    #[test]
    fn plain_json_from_before_gzip_still_decodes() {
        let encoded = STANDARD.encode(sample_release().to_string());
        let details = decode(encoded.as_bytes()).unwrap();
        assert_eq!(&*details.name, "web");
    }

    #[test]
    fn line_breaks_in_the_text_are_ignored() {
        let mut encoded = helm_encode(&sample_release());
        encoded.insert_str(40, "\r\n");
        encoded.push('\n');
        assert_eq!(&*decode(encoded.as_bytes()).unwrap().name, "web");
    }

    #[test]
    fn missing_fields_come_back_empty() {
        let details = decode(helm_encode(&json!({})).as_bytes()).unwrap();
        assert_eq!(details.revision, 0);
        assert_eq!(details.status, HelmStatus::Unknown);
        assert_eq!(details.chart, HelmChart::default());
        assert_eq!(details.updated, None);
        assert_eq!(&*details.values, "");
    }

    #[test]
    fn null_values_mean_none() {
        let mut release = sample_release();
        release["config"] = serde_json::Value::Null;
        release["chart"] = serde_json::Value::Null;
        let details = decode(helm_encode(&release).as_bytes()).unwrap();
        assert_eq!(&*details.values, "");
        assert_eq!(details.chart, HelmChart::default());
    }

    #[test]
    fn values_are_sorted_at_every_level() {
        let mut config = Map::new();
        config.insert("zeta".into(), json!({ "y": 1, "b": [{ "q": 1, "c": 2 }] }));
        config.insert("alpha".into(), json!(true));
        assert_eq!(
            &*values_yaml(Some(&config)).unwrap(),
            // `y` is quoted because YAML 1.1 reads a bare `y` as true.
            "alpha: true\nzeta:\n  b:\n  - c: 2\n    q: 1\n  \"y\": 1\n"
        );
    }

    #[test]
    fn a_bad_time_does_not_hide_the_release() {
        let mut release = sample_release();
        release["info"]["last_deployed"] = json!("yesterday");
        let details = decode(helm_encode(&release).as_bytes()).unwrap();
        assert_eq!(details.updated, None);
        assert!(details.first_deployed.is_some());
    }

    #[test]
    fn go_zero_time_is_no_time() {
        assert_eq!(instant(Some("0001-01-01T00:00:00Z")), None);
        assert_eq!(
            instant(Some("1970-01-01T00:00:10Z")),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
        );
    }

    #[test]
    fn missing_data_is_reported() {
        let mut secret = secret_with(b"");
        secret.data = None;
        assert!(matches!(decode_secret(&secret), Err(DecodeError::Missing)));
        assert!(matches!(
            decode_config_map(&ConfigMap::default()),
            Err(DecodeError::Missing)
        ));
    }

    #[test]
    fn corrupt_payloads_give_clear_errors() {
        let not_base64 = decode(b"this is not base64!").unwrap_err();
        assert!(matches!(not_base64, DecodeError::Base64(_)));
        assert_eq!(not_base64.to_string(), "the payload is not valid base64");

        let mut truncated = gzip(sample_release().to_string().as_bytes());
        truncated.truncate(truncated.len() / 2);
        let bad_gzip = decode(STANDARD.encode(truncated).as_bytes()).unwrap_err();
        assert!(matches!(bad_gzip, DecodeError::Gzip(_)));
        assert_eq!(bad_gzip.to_string(), "the payload is not valid gzip");

        let not_json = decode(STANDARD.encode(gzip(b"<html>")).as_bytes()).unwrap_err();
        assert!(matches!(not_json, DecodeError::Json(_)));
        assert_eq!(not_json.to_string(), "the payload is not a Helm release");

        let wrong_shape = decode(helm_encode(&json!({ "version": "three" })).as_bytes());
        assert!(matches!(wrong_shape, Err(DecodeError::Json(_))));
    }

    #[test]
    fn oversized_payloads_are_refused() {
        let packed = gzip(&[b' '; 4096]);
        assert!(matches!(
            unpack(packed.clone(), 1024),
            Err(DecodeError::TooLarge)
        ));
        assert_eq!(unpack(packed, 4096).unwrap().len(), 4096);
    }
}
