use muse_local_tools::*;

#[test]
fn registered_commands_alone_discover_grants_without_private_paths() {
    // Only commands_v2 crosses the registration boundary; a supplemental
    // top-level grants array cannot make these capabilities discoverable.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let broker = ToolBroker::new();
    let grants = [
        broker.grant(first.path()).unwrap(),
        broker.grant(second.path()).unwrap(),
    ];
    let manifest = broker.manifest();
    let commands = &manifest["commands_v2"];
    for command in ["files.search", "files.read_text", "files.create_text"] {
        let parameter = &commands[command]["required"]["grant_id"];
        let description = parameter["description"].as_str().unwrap();
        for grant in &grants {
            assert!(
                description.contains(&grant.id),
                "registered parameter omits usable grant ID"
            );
            assert!(description.contains(&grant.label));
            assert!(parameter["enum"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(grant.id)));
        }
        assert_eq!(
            parameter,
            &commands[command]["parameters"]["properties"]["grant_id"]
        );
    }
    let encoded = commands.to_string();
    assert!(!encoded.contains(first.path().to_str().unwrap()));
    assert!(!encoded.contains(second.path().to_str().unwrap()));
    broker.revoke_all();
    let after_revoke = broker.manifest()["commands_v2"].to_string();
    for grant in grants {
        assert!(!after_revoke.contains(&grant.id));
    }
}
#[test]
fn manifest_uses_sdk_required_optional_without_private_roots() {
    let d = tempfile::tempdir().unwrap();
    let b = ToolBroker::new();
    let g = b.grant(d.path()).unwrap();
    let manifest = b.manifest();
    assert!(!manifest.to_string().contains(d.path().to_str().unwrap()));
    assert_eq!(manifest["grants"][0]["id"], g.id);
    for command in ["files.search", "files.read_text", "files.create_text"] {
        assert_eq!(
            manifest["commands_v2"][command]["required"]["grant_id"]["type"],
            "string"
        );
        assert!(manifest["commands_v2"][command]["optional"].is_object());
        assert_eq!(
            manifest["commands_v2"][command]["parameters"]["additionalProperties"],
            false
        );
    }
    assert!(manifest["commands_v2"]["system.run"].is_null());
}
