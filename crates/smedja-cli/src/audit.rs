use serde_json::json;
pub(crate) fn build_audit_params(
    path: Option<&str>,
    branch: Option<&str>,
    pr: Option<&str>,
    diff: bool,
    report: Option<&str>,
    format: &str,
    workspace: Option<&str>,
) -> serde_json::Value {
    let mut params = json!({ "format": format });
    if let Some(pr) = pr {
        params["pr"] = json!(pr);
    } else if let Some(base) = branch {
        params["branch"] = json!(base);
    } else if diff {
        params["diff"] = json!(true);
    } else if let Some(path) = path {
        params["path"] = json!(path);
    }
    if let Some(report) = report {
        params["report"] = json!(report);
    }
    if let Some(ws) = workspace {
        params["workspace"] = json!(ws);
    }
    params
}
