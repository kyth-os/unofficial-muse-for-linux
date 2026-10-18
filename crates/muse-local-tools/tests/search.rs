use muse_local_tools::*;
use serde_json::json;
fn search(b: &ToolBroker, g: &GrantInfo, id: &str, query: &str) -> ToolReply {
    match b.invoke(Invocation {
        id: id.into(),
        command: "files.search".into(),
        params: json!({"grant_id":g.id,"query":query}),
        timeout_ms: 10000,
    }) {
        Decision::Reply(r) => r,
        _ => panic!(),
    }
}
#[test]
fn recursive_filename_search_uses_relative_names() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("sub")).unwrap();
    std::fs::write(d.path().join("sub/needle.txt"), "private").unwrap();
    std::fs::write(d.path().join("other"), "needle").unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    let r = search(&b, &g, "s", "needle");
    assert!(r.ok, "{:?}", r.error);
    assert_eq!(r.output["paths"], json!(["sub/needle.txt"]));
    assert!(!r.output.to_string().contains("private"));
}
#[test]
fn result_scan_and_depth_limits_are_bounded() {
    let d = tempfile::tempdir().unwrap();
    for i in 0..101 {
        std::fs::write(d.path().join(format!("hit{i}")), "").unwrap();
    }
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    let r = search(&b, &g, "results", "hit");
    assert!(r.ok);
    assert_eq!(r.output["paths"].as_array().unwrap().len(), 100);
    assert_eq!(r.output["truncated"], true);
    for i in 101..10_002 {
        std::fs::write(d.path().join(format!("entry{i}")), "").unwrap();
    }
    let r = search(&b, &g, "scans", "not_present");
    assert!(r.ok);
    assert_eq!(r.output["scanned"], 10000);
    assert_eq!(r.output["truncated"], true);
    let deep = tempfile::tempdir().unwrap();
    let mut path = deep.path().to_path_buf();
    for _ in 0..65 {
        path.push("d");
        std::fs::create_dir(&path).unwrap();
    }
    let g = b.grant(deep.path()).unwrap();
    let r = search(&b, &g, "depth", "");
    assert!(!r.ok);
    assert_eq!(r.error.as_deref(), Some("depth_limit"));
    assert!(r.output.is_null());
}
#[test]
fn searches_do_not_share_directory_offsets() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("name"), "").unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    assert_eq!(
        search(&b, &g, "one", "").output["paths"],
        search(&b, &g, "two", "").output["paths"]
    );
}
