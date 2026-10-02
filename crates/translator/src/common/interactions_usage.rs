//! Interactions API usage lookup (Go: common/interactions_usage.go).

use cpa_json::Res;

/// The usage object of an Interactions response/event: the first existing of `interaction.usage`,
/// `usage`, `metadata.total_usage`, `metadata.usage`, `interaction.metadata.total_usage`,
/// `interaction.metadata.usage`. [`Res::NONE`] when there is none.
pub fn interactions_usage<'a>(root: &'a Res<'_>) -> Res<'a> {
    for path in [
        "interaction.usage",
        "usage",
        "metadata.total_usage",
        "metadata.usage",
        "interaction.metadata.total_usage",
        "interaction.metadata.usage",
    ] {
        let value = root.get(path);
        if value.exists() {
            return value;
        }
    }
    Res::NONE
}
