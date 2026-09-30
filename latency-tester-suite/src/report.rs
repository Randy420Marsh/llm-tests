//! Reading saved result files for the web viewer, and exporting them as one self-contained HTML page.

use ed25519_dalek::SigningKey;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

use crate::result_logger::{ResultLogger, VerifiedResult};

/// The viewer page (no external resources: works offline and can be e-mailed)
pub const VIEWER_HTML: &str = include_str!("web/viewer.html");
const EMBED_MARKER: &str = "/*__EMBEDDED__*/null";

#[derive(Debug, Clone, Serialize)]
pub struct ResultMeta {
    pub name: String,
    pub timestamp: String,
    pub test_type: String,
    pub app_version: String,
    /// The Ed25519 signature matches the data (nothing was edited)
    pub valid: Option<bool>,
    /// The signing key is this machine's own key (`signing_key.bin` next to the results)
    pub trusted: Option<bool>,
    pub message: String,
}

/// Only plain `something.json` names may be requested: no paths, no `..`
pub fn is_safe_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() < 200
        && n.ends_with(".json")
        && !n.contains("..")
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

pub fn local_public_key(dir: &Path) -> Option<[u8; 32]> {
    let seed: [u8; 32] = std::fs::read(dir.join("signing_key.bin")).ok()?.try_into().ok()?;
    Some(SigningKey::from_bytes(&seed).verifying_key().to_bytes())
}

pub fn meta_of(name: &str, doc: &VerifiedResult, local_pk: Option<[u8; 32]>) -> ResultMeta {
    let embedded: Option<[u8; 32]> = hex::decode(&doc.signature.public_key).ok().and_then(|b| b.try_into().ok());
    let (valid, message) = match embedded {
        Some(pk) => {
            let v = ResultLogger::verify_with_public_key(doc, &pk);
            (Some(v.valid), v.message)
        }
        None => (Some(false), "malformed public key".to_string()),
    };
    ResultMeta {
        name: name.to_string(),
        timestamp: doc.header.timestamp.clone(),
        test_type: doc.payload.test_type.clone(),
        app_version: doc.header.app_version.clone(),
        valid,
        trusted: match (embedded, local_pk) {
            (Some(e), Some(l)) => Some(e == l),
            _ => None,
        },
        message,
    }
}

/// Every parsable result file in `dir`, newest first
pub fn list(dir: &Path) -> Vec<(ResultMeta, Value)> {
    let local = local_public_key(dir);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !is_safe_name(&name) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(doc) = serde_json::from_str::<VerifiedResult>(&text) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        out.push((meta_of(&name, &doc, local), value));
    }
    out.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp));
    out
}

/// The viewer for the live server: nothing embedded, it fetches the list from the API
pub fn render_live() -> String {
    VIEWER_HTML.replacen(EMBED_MARKER, "null", 1)
}

/// One HTML file containing the viewer and the given results
pub fn render_static(entries: &[(ResultMeta, Value)]) -> String {
    let arr: Vec<Value> = entries
        .iter()
        .map(|(m, d)| serde_json::json!({ "meta": m, "doc": d }))
        .collect();
    let mut json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into());
    // keep the JSON from ending the <script> element or opening an HTML comment
    json = json.replace("</", "<\\/").replace("<!--", "<\\!--");
    VIEWER_HTML.replacen(EMBED_MARKER, &json, 1)
}

/// Load individual files (not a folder) for `--report file.json ...`
pub fn load_files(paths: &[std::path::PathBuf]) -> Vec<(ResultMeta, Value)> {
    let mut out = Vec::new();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        let (Ok(doc), Ok(value)) = (serde_json::from_str::<VerifiedResult>(&text), serde_json::from_str::<Value>(&text)) else { continue };
        let local = p.parent().and_then(local_public_key);
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "result.json".into());
        out.push((meta_of(&name, &doc, local), value));
    }
    out.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A signed session-shaped result in `dir`
    pub fn make_result(dir: &Path, results: Value) -> (ResultLogger, VerifiedResult) {
        let logger = ResultLogger::new("1.0.0".into(), dir.to_string_lossy().into()).unwrap();
        let info = crate::system_info::collect_system_info().unwrap();
        let v = logger.log_result("session", &info, &serde_json::json!({}), &results, HashMap::new()).unwrap();
        (logger, v)
    }

    #[test]
    fn safe_names_only() {
        assert!(is_safe_name("2026-09-30T05-50-54_cpu_7737aba9.json"));
        assert!(is_safe_name("2026-09-30T05-50-54-802922100+00-00_cpu_7737aba9.json"));
        assert!(!is_safe_name("../secret.json"));
        assert!(!is_safe_name("a/b.json"));
        assert!(!is_safe_name("a\\b.json"));
        assert!(!is_safe_name("signing_key.bin"));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name("x..json"));
    }

    #[test]
    fn list_verifies_signatures_and_flags_edits() {
        let dir = tempfile::tempdir().unwrap();
        let (_l, good) = make_result(dir.path(), serde_json::json!({"memory": {"results": []}}));
        // an edited copy of the file (signature unchanged)
        let mut bad = good.clone();
        bad.payload.benchmark_results = serde_json::json!({"memory": {"results": [1, 2, 3]}});
        bad.header.timestamp = "2099-01-01T00:00:00Z".into();
        std::fs::write(dir.path().join("edited.json"), serde_json::to_string(&bad).unwrap()).unwrap();
        std::fs::write(dir.path().join("junk.json"), "not json").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let items = list(dir.path());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0.name, "edited.json"); // newest first
        assert_eq!(items[0].0.valid, Some(false));
        assert!(items[0].0.message.contains("tampered"));
        let g = items.iter().find(|i| i.0.name != "edited.json").unwrap();
        assert_eq!(g.0.valid, Some(true));
        assert_eq!(g.0.trusted, Some(true)); // signed with this folder's key
    }

    #[test]
    fn foreign_key_is_valid_but_not_trusted() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (_l, v) = make_result(a.path(), serde_json::json!({"cpu": {"results": []}}));
        std::fs::write(b.path().join("from_other_pc.json"), serde_json::to_string(&v).unwrap()).unwrap();
        // b has its own key
        ResultLogger::new("1".into(), b.path().to_string_lossy().into()).unwrap();
        let items = list(b.path());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0.valid, Some(true));
        assert_eq!(items[0].0.trusted, Some(false));
    }

    #[test]
    fn static_report_embeds_data_safely() {
        let dir = tempfile::tempdir().unwrap();
        let (_l, _v) = make_result(dir.path(), serde_json::json!({"note": "</script><script>alert(1)</script><!--"}));
        let html = render_static(&list(dir.path()));
        assert!(!html.contains("/*__EMBEDDED__*/null"));
        assert!(html.contains("\"test_type\":\"session\""));
        assert!(!html.contains("</script><script>alert(1)"));
        assert!(html.contains("<\\/script>"));
        // exactly one closing tag for the embedded block plus the main script
        assert_eq!(html.matches("</script>").count(), 2);
    }

    #[test]
    fn live_page_has_no_leftover_placeholder() {
        let html = render_live();
        assert!(!html.contains("__EMBEDDED__"));
        assert!(html.contains("<script id=\"embedded\" type=\"application/json\">null</script>"));
    }

    #[test]
    fn load_files_reads_single_documents() {
        let dir = tempfile::tempdir().unwrap();
        let (_l, _v) = make_result(dir.path(), serde_json::json!({}));
        let file = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().map_or(false, |e| e == "json")).unwrap();
        let items = load_files(&[file, "/nope.json".into()]);
        assert_eq!(items.len(), 1);
    }
}
