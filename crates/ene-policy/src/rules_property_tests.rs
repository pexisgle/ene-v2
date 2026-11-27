use super::*;
use proptest::prelude::*;
use serde_json::json;

/// A JSON value of arbitrary shape, including nested objects and arrays.
fn any_json() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(|n| json!(n)),
        any::<bool>().prop_map(|b| json!(b)),
        "[a-z]{0,8}".prop_map(|s| json!(s)),
        Just(serde_json::Value::Null),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(|v| json!(v)),
            prop::collection::hash_map("[a-z]{1,5}", inner, 0..4).prop_map(|m| json!(m)),
        ]
    })
}

proptest! {
    /// The same value always canonicalises to the same bytes. The gate
    /// compares digests, so a non-deterministic form would make a saved
    /// allow rule stop matching the call it was saved for.
    #[test]
    fn canonicalisation_is_deterministic(value in any_json(), tool in "[a-z]{1,8}") {
        let (first_text, first_hash) = canonicalise(&tool, &value);
        let (second_text, second_hash) = canonicalise(&tool, &value);
        prop_assert_eq!(first_text, second_text);
        prop_assert_eq!(first_hash, second_hash);
    }

    /// Key insertion order must not matter. Two objects with the same
    /// members in a different order are the same call, and must produce
    /// the same digest.
    #[test]
    fn key_order_does_not_change_the_digest(
        a in "[a-z]{1,4}",
        b in "[a-z]{1,4}",
        x in any::<i64>(),
        y in any::<i64>(),
    ) {
        prop_assume!(a != b);
        let mut first = serde_json::Map::new();
        first.insert(a.clone(), json!(x));
        first.insert(b.clone(), json!(y));
        let mut second = serde_json::Map::new();
        second.insert(b, json!(y));
        second.insert(a, json!(x));
        let (_, first_hash) = canonicalise("t", &serde_json::Value::Object(first));
        let (_, second_hash) = canonicalise("t", &serde_json::Value::Object(second));
        prop_assert_eq!(first_hash, second_hash);
    }

    /// The tool name is part of the identity: the same arguments under a
    /// different tool must not collide, or an approval for one tool would
    /// authorise another.
    #[test]
    fn the_tool_name_separates_otherwise_equal_calls(
        value in any_json(),
        left in "[a-z]{1,6}",
        right in "[a-z]{1,6}",
    ) {
        prop_assume!(left != right);
        let (_, first) = canonicalise(&left, &value);
        let (_, second) = canonicalise(&right, &value);
        prop_assert_ne!(first, second);
    }

    /// The digest feeds the approval payload hash, so its bytes are pinned:
    /// a different encoding would silently invalidate every stored
    /// `exact_sha256:` approval.
    #[test]
    fn hex_digests_match_the_pinned_encoding(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let expected: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        prop_assert_eq!(hex_digest(&bytes), expected);
    }
}
