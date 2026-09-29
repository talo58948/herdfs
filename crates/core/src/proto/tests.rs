use super::*;

#[test]
fn requests_are_tagged_by_op() {
    let request = Request::Stat {
        space: "team".into(),
        path: SpacePath::try_from("a/b").unwrap(),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"op": "stat", "space": "team", "path": "a/b"})
    );
    assert_eq!(serde_json::from_value::<Request>(json).unwrap(), request);
}

#[test]
fn invalid_paths_fail_to_decode() {
    let json = serde_json::json!({"op": "stat", "space": "team", "path": "../etc"});
    assert!(serde_json::from_value::<Request>(json).is_err());
}
