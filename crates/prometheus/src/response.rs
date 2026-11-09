//! Reading Prometheus HTTP API responses.
//!
//! A range query answers with `{"status":"success","data":{"resultType":
//! "matrix","result":[{"metric":{...},"values":[[time,"value"],...]}]}}`.
//! Errors carry `errorType` and `error`. VictoriaMetrics, Mimir and Thanos
//! answer the same way.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use model::{MetricSeries, Sample};
use serde_json::Value;

use crate::query::QueryError;

/// The longest piece of an unexpected body kept in an error message.
const SNIPPET_LEN: usize = 200;

/// Parses a successful range query body.
pub(crate) fn parse_matrix(body: &[u8]) -> Result<Vec<MetricSeries>, QueryError> {
    let root: Value = serde_json::from_slice(body)
        .map_err(|e| QueryError::Decode(format!("not JSON ({e}): {}", snippet(body))))?;
    if root["status"] != "success" {
        return Err(prometheus_error(200, &root).unwrap_or_else(|| {
            QueryError::Decode(format!("no success status: {}", snippet(body)))
        }));
    }
    if let Some(warnings) = root["warnings"].as_array() {
        for warning in warnings {
            tracing::debug!("Prometheus warning: {warning}");
        }
    }
    let data = &root["data"];
    let kind = data["resultType"].as_str().unwrap_or_default();
    if kind != "matrix" {
        return Err(QueryError::Decode(format!(
            "expected a matrix result, got `{kind}`"
        )));
    }
    let Some(result) = data["result"].as_array() else {
        return Err(QueryError::Decode("the result is not a list".to_owned()));
    };
    result.iter().map(series).collect()
}

fn series(item: &Value) -> Result<MetricSeries, QueryError> {
    let labels: BTreeMap<String, String> = item["metric"]
        .as_object()
        .map(|metric| {
            metric
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let values = item["values"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let samples = values.iter().map(sample).collect::<Result<Vec<_>, _>>()?;
    Ok(MetricSeries { labels, samples })
}

/// One `[time, "value"]` pair. Time is float seconds since the epoch.
fn sample(pair: &Value) -> Result<Sample, QueryError> {
    let bad = || QueryError::Decode(format!("bad sample {pair}"));
    let time = pair[0].as_f64().ok_or_else(bad)?;
    // Values are strings so that NaN and +Inf survive JSON. Rust's parser
    // reads those spellings.
    let value: f64 = pair[1]
        .as_str()
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let offset = Duration::try_from_secs_f64(time).map_err(|_| bad())?;
    let time = SystemTime::UNIX_EPOCH.checked_add(offset).ok_or_else(bad)?;
    Ok(Sample { time, value })
}

/// Reads an error body: Prometheus's own error, a Kubernetes `Status` from
/// the API server, or plain text from a proxy.
pub(crate) fn parse_error(status: u16, body: &[u8]) -> QueryError {
    let Ok(root) = serde_json::from_slice::<Value>(body) else {
        return QueryError::Http {
            status,
            message: snippet(body),
        };
    };
    if let Some(error) = prometheus_error(status, &root) {
        return error;
    }
    if root["kind"] == "Status"
        && let Some(message) = root["message"].as_str()
    {
        return QueryError::Http {
            status,
            message: message.to_owned(),
        };
    }
    QueryError::Http {
        status,
        message: snippet(body),
    }
}

fn prometheus_error(status: u16, root: &Value) -> Option<QueryError> {
    if root["status"] != "error" {
        return None;
    }
    Some(QueryError::Prometheus {
        status,
        kind: root["errorType"].as_str().unwrap_or("unknown").to_owned(),
        message: root["error"].as_str().unwrap_or_default().to_owned(),
    })
}

/// The start of a body, trimmed, for error messages.
fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    match text.char_indices().nth(SNIPPET_LEN) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs)
    }

    #[test]
    fn reads_a_matrix() {
        let body = br#"{"status":"success","data":{"resultType":"matrix","result":[
            {"metric":{"pod":"web-1","namespace":"shop"},"values":[[1762650000,"0.25"],[1762650060.5,"0.5"]]},
            {"metric":{},"values":[]}
        ]}}"#;
        let series = parse_matrix(body).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].labels["pod"], "web-1");
        assert_eq!(series[0].labels["namespace"], "shop");
        assert_eq!(
            series[0].samples,
            vec![
                Sample {
                    time: at(1_762_650_000.0),
                    value: 0.25
                },
                Sample {
                    time: at(1_762_650_060.5),
                    value: 0.5
                },
            ]
        );
        assert_eq!(series[1], MetricSeries::default());
    }

    #[test]
    fn keeps_nan_and_infinity() {
        let body = br#"{"status":"success","data":{"resultType":"matrix","result":[
            {"metric":{},"values":[[1,"NaN"],[2,"+Inf"],[3,"-Inf"]]}
        ]}}"#;
        let values: Vec<f64> = parse_matrix(body).unwrap()[0]
            .samples
            .iter()
            .map(|s| s.value)
            .collect();
        assert!(values[0].is_nan());
        assert_eq!(values[1], f64::INFINITY);
        assert_eq!(values[2], f64::NEG_INFINITY);
    }

    #[test]
    fn an_instant_result_is_not_a_range() {
        let body = br#"{"status":"success","data":{"resultType":"vector","result":[]}}"#;
        assert_eq!(
            parse_matrix(body),
            Err(QueryError::Decode(
                "expected a matrix result, got `vector`".to_owned()
            ))
        );
    }

    #[test]
    fn html_is_reported_briefly() {
        let body = format!("<html>{}</html>", "x".repeat(500));
        let Err(QueryError::Decode(message)) = parse_matrix(body.as_bytes()) else {
            panic!("expected a decode error");
        };
        assert!(message.len() < 300, "{message}");
        assert!(message.ends_with("..."), "{message}");
    }

    #[test]
    fn reads_prometheus_errors() {
        let body = br#"{"status":"error","errorType":"bad_data","error":"parse error at char 5"}"#;
        assert_eq!(
            parse_error(400, body),
            QueryError::Prometheus {
                status: 400,
                kind: "bad_data".to_owned(),
                message: "parse error at char 5".to_owned(),
            }
        );
    }

    #[test]
    fn reads_api_server_statuses() {
        let body = br#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"no endpoints available for service \"prometheus\"","reason":"ServiceUnavailable","code":503}"#;
        assert_eq!(
            parse_error(503, body),
            QueryError::Http {
                status: 503,
                message: r#"no endpoints available for service "prometheus""#.to_owned(),
            }
        );
    }

    #[test]
    fn keeps_plain_text_errors() {
        assert_eq!(
            parse_error(502, b"Bad Gateway\n"),
            QueryError::Http {
                status: 502,
                message: "Bad Gateway".to_owned(),
            }
        );
    }
}
