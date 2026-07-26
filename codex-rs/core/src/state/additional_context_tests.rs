use std::collections::BTreeMap;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS;
use codex_utils_string::approx_bytes_for_tokens;
use pretty_assertions::assert_eq;

use super::AdditionalContextStore;

fn application(value: impl Into<String>) -> AdditionalContextEntry {
    AdditionalContextEntry {
        value: value.into(),
        kind: AdditionalContextKind::Application,
    }
}

#[test]
fn committed_application_receipts_are_idempotent_and_conflicts_fail() {
    let mut store = AdditionalContextStore::default();
    let receipt = BTreeMap::from([("receipt_one".to_string(), application("correction"))]);

    assert_eq!(store.commit_application(receipt.clone()).unwrap().len(), 1);
    assert_eq!(store.commit_application(receipt).unwrap(), Vec::new());
    assert_eq!(
        store
            .commit_application(BTreeMap::from([(
                "receipt_one".to_string(),
                application("different correction"),
            )]))
            .unwrap_err()
            .key,
        "receipt_one"
    );
}

#[test]
fn application_fragment_conversion_truncates_values_above_the_shared_budget() {
    let mut store = AdditionalContextStore::default();
    let value = "x".repeat(approx_bytes_for_tokens(
        MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS + 1,
    ));
    let item = store
        .commit_application(BTreeMap::from([(
            "oversize_receipt".to_string(),
            application(value.clone()),
        )]))
        .unwrap()
        .pop()
        .expect("new receipt");
    let ResponseItem::Message { content, .. } = ResponseItem::from(item) else {
        panic!("Application context must render as a developer message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("Application context must render as one text item");
    };

    assert!(text.len() < value.len());
    assert!(text.contains("tokens truncated"));
}
