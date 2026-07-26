use std::collections::BTreeMap;

use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplicationContextReceiptConflict {
    pub(crate) key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: BTreeMap<String, AdditionalContextEntry>,
    committed_application_receipts: BTreeMap<String, String>,
}

impl AdditionalContextStore {
    pub(crate) fn merge(
        &mut self,
        values: BTreeMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        let fragments = values
            .iter()
            .filter(|(key, value)| self.values.get(*key) != Some(*value))
            .map(|(key, entry)| match entry.kind {
                AdditionalContextKind::Untrusted => {
                    AdditionalContextUserFragment::new(key.clone(), entry.value.clone())
                        .into_response_input_item()
                }
                AdditionalContextKind::Application => {
                    AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone())
                        .into_response_input_item()
                }
            })
            .collect();
        self.values = values;
        fragments
    }

    /// Materialize an Application-only, context-only delivery exactly once per key.
    ///
    /// Unlike the snapshot-oriented `values` map, committed receipt memory survives later
    /// additional-context snapshots. This lets a caller safely retry after losing an RPC response
    /// without injecting the same developer message twice.
    pub(crate) fn commit_application(
        &mut self,
        values: BTreeMap<String, AdditionalContextEntry>,
    ) -> Result<Vec<ResponseInputItem>, ApplicationContextReceiptConflict> {
        debug_assert!(
            !values.is_empty()
                && values
                    .values()
                    .all(|entry| entry.kind == AdditionalContextKind::Application)
        );
        if let Some((key, _)) = values.iter().find(|(key, entry)| {
            self.committed_application_receipts
                .get(*key)
                .is_some_and(|value| value != &entry.value)
        }) {
            return Err(ApplicationContextReceiptConflict { key: key.clone() });
        }

        let fragments = values
            .iter()
            .filter(|(key, entry)| {
                self.committed_application_receipts.get(*key) != Some(&entry.value)
            })
            .map(|(key, entry)| {
                AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone())
                    .into_response_input_item()
            })
            .collect::<Vec<_>>();

        for (key, entry) in &values {
            if self.committed_application_receipts.contains_key(key) {
                continue;
            }
            self.committed_application_receipts
                .insert(key.clone(), entry.value.clone());
        }
        self.values = values;
        Ok(fragments)
    }
}

#[cfg(test)]
#[path = "additional_context_tests.rs"]
mod tests;
