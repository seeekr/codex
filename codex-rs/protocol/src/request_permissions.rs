use crate::models::AdditionalPermissionProfile;
use crate::models::FileSystemPermissions;
use crate::models::NetworkPermissions;
use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantScope {
    #[default]
    Turn,
    Session,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RequestPermissionProfile {
    pub network: Option<NetworkPermissions>,
    pub file_system: Option<FileSystemPermissions>,
}

impl RequestPermissionProfile {
    pub fn is_empty(&self) -> bool {
        self.network.is_none() && self.file_system.is_none()
    }
}

impl From<RequestPermissionProfile> for AdditionalPermissionProfile {
    fn from(value: RequestPermissionProfile) -> Self {
        Self {
            network: value.network,
            file_system: value.file_system,
        }
    }
}

impl From<AdditionalPermissionProfile> for RequestPermissionProfile {
    fn from(value: AdditionalPermissionProfile) -> Self {
        Self {
            network: value.network,
            file_system: value.file_system,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestPermissionsArgs {
    #[serde(
        default,
        rename = "environment_id",
        alias = "environmentId",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub environment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
}

/// A fixed-purpose automatic reviewer failure that prevented a permission
/// decision. This is intentionally separate from an empty permission grant:
/// callers must not interpret reviewer infrastructure failure as a risk or
/// user denial.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestPermissionsReviewFailure {
    ReviewerUnavailable { message: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestPermissionsResponse {
    pub permissions: RequestPermissionProfile,
    #[serde(default)]
    pub scope: PermissionGrantScope,
    /// Review subsequent commands in this turn unless a permission hook resolves the request.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict_auto_review: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub review_failure: Option<RequestPermissionsReviewFailure>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestPermissionsEvent {
    /// Responses API call id for the associated tool call, if available.
    pub call_id: String,
    /// Turn ID that this request belongs to.
    /// Uses `#[serde(default)]` for backwards compatibility.
    #[serde(default)]
    pub turn_id: String,
    #[serde(
        default,
        rename = "environmentId",
        alias = "environment_id",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    #[ts(rename = "environmentId")]
    pub environment_id: Option<String>,
    #[ts(type = "number")]
    pub started_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cwd: Option<AbsolutePathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn reviewer_unavailable_failure_round_trips_with_retry_guidance() {
        let value = serde_json::to_value(RequestPermissionsResponse {
            permissions: RequestPermissionProfile::default(),
            scope: PermissionGrantScope::Turn,
            strict_auto_review: false,
            review_failure: Some(RequestPermissionsReviewFailure::ReviewerUnavailable {
                message: "temporarily unavailable; retry with backoff".to_string(),
            }),
        })
        .expect("serialize request permissions response");

        assert_eq!(
            value["review_failure"],
            json!({
                "kind": "reviewer_unavailable",
                "message": "temporarily unavailable; retry with backoff",
            })
        );
        assert_eq!(
            serde_json::from_value::<RequestPermissionsResponse>(value)
                .expect("deserialize request permissions response")
                .review_failure,
            Some(RequestPermissionsReviewFailure::ReviewerUnavailable {
                message: "temporarily unavailable; retry with backoff".to_string(),
            })
        );
    }

    #[test]
    fn legacy_response_without_review_failure_remains_compatible() {
        let response = serde_json::from_value::<RequestPermissionsResponse>(json!({
            "permissions": {},
            "scope": "turn",
        }))
        .expect("deserialize legacy request permissions response");

        assert_eq!(response.review_failure, None);
        assert!(!response.strict_auto_review);
    }
}
