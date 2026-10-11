use buffa::Message as _;
use ene_rpc::{
    ConnectionInputs, JsonDocument, JsonDocumentPatch, UpdateCompanionRequest,
    UpdateSettingsRequest, json_document_patch,
};

#[test]
fn connection_omission_and_empty_replacement_survive_both_codecs() {
    let omitted = UpdateSettingsRequest {
        revision: 1,
        ..Default::default()
    };
    let empty = UpdateSettingsRequest {
        connections: ConnectionInputs::default().into(),
        ..omitted.clone()
    };
    assert_ne!(omitted.encode_to_vec(), empty.encode_to_vec());
    for value in [omitted, empty] {
        let bytes = value.encode_to_vec();
        assert_eq!(
            UpdateSettingsRequest::decode_from_slice(&bytes).unwrap(),
            value
        );
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(
            serde_json::from_str::<UpdateSettingsRequest>(&json).unwrap(),
            value
        );
    }
    let old_json = r#"{"revision":"1","connections":[{"id":"old"}]}"#;
    assert!(serde_json::from_str::<UpdateSettingsRequest>(old_json).is_err());
}

#[test]
fn body_and_voice_patches_preserve_set_clear_and_omission() {
    let request = UpdateCompanionRequest {
        id: "c1".into(),
        revision: 1,
        body: JsonDocumentPatch {
            change: Some(json_document_patch::Change::Set(Box::new(JsonDocument {
                json: r#"{"format":"vrm1","environment_id":"env1","model":"avatar.vrm"}"#.into(),
                ..Default::default()
            }))),
            ..Default::default()
        }
        .into(),
        voice: JsonDocumentPatch {
            change: Some(json_document_patch::Change::Clear(Box::default())),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    assert_eq!(
        UpdateCompanionRequest::decode_from_slice(&request.encode_to_vec()).unwrap(),
        request
    );
    assert_eq!(
        serde_json::from_str::<UpdateCompanionRequest>(&serde_json::to_string(&request).unwrap())
            .unwrap(),
        request
    );
    let omitted: UpdateCompanionRequest = serde_json::from_str("{}").unwrap();
    let null: UpdateCompanionRequest =
        serde_json::from_str(r#"{"body":null,"voice":null}"#).unwrap();
    assert_eq!(null, omitted);
    assert!(!omitted.body.is_set());
    assert!(!omitted.voice.is_set());
    assert!(serde_json::from_str::<JsonDocumentPatch>(r#"{"set":{},"clear":{}}"#).is_err());
}
