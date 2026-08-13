/// Whether a tab's reported position can be trusted enough to address a
/// `rename_tab` call at it.
///
/// `TabInfo::position` arrives in a snapshot that may predate a tab close, and
/// upstream `zellij-org/zellij#3535` lets the server's positions drift from the
/// plugin's. When `position` disagrees with the tab's actual index in the list
/// we just received, positions are mid-drift and a rename could land on the
/// wrong tab. Extracted as a pure function so the rule is unit-testable
/// without a live zellij.
pub fn position_is_addressable(reported_position: usize, index_in_list: usize) -> bool {
    reported_position == index_in_list
}

/// Whether a desired rename has exhausted its retry budget.
///
/// Returns true when the same desired name has already been issued
/// `max_attempts` times without landing, meaning it should be abandoned rather
/// than re-issued forever (which is what turns `#3535` into a server-pegging
/// feedback loop).
pub fn rename_budget_exhausted(
    prior_desired: Option<&str>,
    desired: &str,
    attempts: u8,
    max_attempts: u8,
) -> bool {
    match prior_desired {
        // A different target resets the budget.
        Some(prior) if prior != desired => false,
        Some(_) => attempts >= max_attempts,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_trusted_when_snapshot_agrees_with_list_order() {
        assert!(position_is_addressable(0, 0));
        assert!(position_is_addressable(7, 7));
    }

    #[test]
    fn position_rejected_when_positions_have_drifted() {
        // The #3535 signature: a close happened, so the reported position no
        // longer matches where the tab actually sits in the fresh list.
        assert!(!position_is_addressable(5, 4));
        assert!(!position_is_addressable(0, 3));
    }

    #[test]
    fn first_attempt_at_a_name_is_always_allowed() {
        assert!(!rename_budget_exhausted(None, "build", 0, 3));
    }

    #[test]
    fn retries_allowed_up_to_the_budget_then_abandoned() {
        assert!(!rename_budget_exhausted(Some("build"), "build", 1, 3));
        assert!(!rename_budget_exhausted(Some("build"), "build", 2, 3));
        // Budget reached: stop re-issuing. This is the brake that prevents the
        // rename/TabUpdate feedback loop.
        assert!(rename_budget_exhausted(Some("build"), "build", 3, 3));
        assert!(rename_budget_exhausted(Some("build"), "build", 9, 3));
    }

    #[test]
    fn a_new_target_resets_the_budget() {
        // Claude changed its OSC title; that is a fresh intent, not a retry, so
        // an exhausted budget for the old name must not suppress it.
        assert!(!rename_budget_exhausted(Some("build"), "deploy", 9, 3));
    }
}
