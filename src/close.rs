use std::collections::BTreeSet;
use zellij_tile::prelude::{PaneInfo, PaneManifest};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CloseScope {
    Pane,
    Tab,
}

#[derive(Debug, PartialEq)]
pub struct CloseRequest {
    pub scope: CloseScope,
    pub pane_id: Option<u32>,
}

impl CloseRequest {
    pub fn parse(payload: Option<&str>) -> Self {
        let mut scope = CloseScope::Pane;
        let mut pane_id = None;
        for tok in payload.unwrap_or("").split_whitespace() {
            match tok {
                "tab" => scope = CloseScope::Tab,
                "pane" => scope = CloseScope::Pane,
                other => {
                    if let Ok(id) = other.parse::<u32>() {
                        pane_id = Some(id);
                    }
                }
            }
        }
        CloseRequest { scope, pane_id }
    }
}

#[derive(Debug)]
pub struct PendingClose {
    pub scope: CloseScope,
    pub tab_index: usize,
    pub targets: Vec<u32>,
    pub awaiting: BTreeSet<u32>,
    pub deadline_ms: u64,
}

fn is_zellaude(pane: &PaneInfo) -> bool {
    pane.is_plugin
        && pane
            .plugin_url
            .as_deref()
            .map_or(false, |url| url.contains("zellaude"))
}

/// Tab holding the given terminal pane. Derived purely from pane structure,
/// which stays consistent across instances — unlike the per-instance active-tab
/// flag, which is stale in background tabs.
pub fn tab_of_terminal(manifest: &PaneManifest, pane_id: u32) -> Option<usize> {
    manifest
        .panes
        .iter()
        .find(|(_, panes)| panes.iter().any(|p| !p.is_plugin && p.id == pane_id))
        .map(|(&tab, _)| tab)
}

/// The single zellaude instance that should carry out this close.
///
/// A tab close must be driven by an instance in a *different* tab: closing its
/// own tab from inside its handler unloads the running plugin mid-call and
/// aborts the whole server (ishefi/zellaude#17). Closing a terminal pane never
/// unloads a plugin, so any instance may do it. Election is deterministic from
/// the manifest so every instance agrees on exactly one actor.
pub fn actor(manifest: &PaneManifest, scope: CloseScope, target_tab: usize) -> Option<u32> {
    let mut outside: Vec<u32> = Vec::new();
    let mut inside: Vec<u32> = Vec::new();
    for (&tab, panes) in &manifest.panes {
        for p in panes.iter().filter(|p| is_zellaude(p)) {
            if tab == target_tab {
                inside.push(p.id);
            } else {
                outside.push(p.id);
            }
        }
    }
    match scope {
        CloseScope::Pane => outside.into_iter().chain(inside).min(),
        CloseScope::Tab => outside.into_iter().min().or_else(|| inside.into_iter().min()),
    }
}

/// The zellaude instance that coordinates a close: the one living in the
/// target tab (it is the focused, lively instance, so it reliably receives the
/// SessionEnd and Timer events that drive the graceful wait).
pub fn brain(manifest: &PaneManifest, target_tab: usize) -> Option<u32> {
    manifest
        .panes
        .get(&target_tab)?
        .iter()
        .filter(|p| is_zellaude(p))
        .map(|p| p.id)
        .min()
}

pub fn plan_close(
    scope: CloseScope,
    manifest: &PaneManifest,
    target_tab: usize,
    target_pane: Option<u32>,
    has_session: impl Fn(u32) -> bool,
    deadline_ms: u64,
) -> Option<PendingClose> {
    let panes = manifest.panes.get(&target_tab)?;
    let targets: Vec<u32> = match scope {
        CloseScope::Pane => vec![target_pane?],
        CloseScope::Tab => panes.iter().filter(|p| !p.is_plugin).map(|p| p.id).collect(),
    };
    if targets.is_empty() {
        return None;
    }
    let awaiting = targets.iter().copied().filter(|&id| has_session(id)).collect();
    Some(PendingClose {
        scope,
        tab_index: target_tab,
        targets,
        awaiting,
        deadline_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pane(id: u32, is_plugin: bool, url: Option<&str>) -> PaneInfo {
        PaneInfo {
            id,
            is_plugin,
            plugin_url: url.map(str::to_owned),
            ..Default::default()
        }
    }

    fn manifest(tabs: Vec<(usize, Vec<PaneInfo>)>) -> PaneManifest {
        PaneManifest {
            panes: tabs.into_iter().collect::<HashMap<_, _>>(),
        }
    }

    const Z: Option<&str> = Some("file:/x/zellaude.wasm");

    #[test]
    fn parse_reads_scope_and_pane_id_in_any_order() {
        assert_eq!(
            CloseRequest::parse(Some("tab 14")),
            CloseRequest { scope: CloseScope::Tab, pane_id: Some(14) }
        );
        assert_eq!(
            CloseRequest::parse(Some("7 pane")),
            CloseRequest { scope: CloseScope::Pane, pane_id: Some(7) }
        );
        assert_eq!(
            CloseRequest::parse(None),
            CloseRequest { scope: CloseScope::Pane, pane_id: None }
        );
    }

    #[test]
    fn tab_of_terminal_finds_the_owning_tab() {
        let m = manifest(vec![
            (0, vec![pane(10, true, Z), pane(2, false, None)]),
            (1, vec![pane(11, true, Z), pane(3, false, None)]),
        ]);
        assert_eq!(tab_of_terminal(&m, 3), Some(1));
        assert_eq!(tab_of_terminal(&m, 99), None);
    }

    #[test]
    fn tab_close_is_delegated_outside_the_target_tab() {
        let m = manifest(vec![
            (0, vec![pane(10, true, Z), pane(2, false, None)]),
            (1, vec![pane(11, true, Z), pane(3, false, None)]),
        ]);
        assert_eq!(actor(&m, CloseScope::Tab, 1), Some(10));
        assert_eq!(actor(&m, CloseScope::Tab, 0), Some(11));
    }

    #[test]
    fn tab_close_falls_back_inside_when_target_is_only_tab() {
        let m = manifest(vec![(0, vec![pane(10, true, Z), pane(2, false, None)])]);
        assert_eq!(actor(&m, CloseScope::Tab, 0), Some(10));
    }

    #[test]
    fn pane_close_is_any_lowest_instance() {
        let m = manifest(vec![
            (0, vec![pane(10, true, Z)]),
            (1, vec![pane(4, true, Z), pane(3, false, None)]),
        ]);
        assert_eq!(actor(&m, CloseScope::Pane, 1), Some(4));
    }

    #[test]
    fn brain_is_the_instance_inside_the_target_tab() {
        let m = manifest(vec![
            (0, vec![pane(10, true, Z), pane(2, false, None)]),
            (1, vec![pane(11, true, Z), pane(3, false, None)]),
        ]);
        assert_eq!(brain(&m, 1), Some(11));
        assert_eq!(brain(&m, 0), Some(10));
        assert_eq!(brain(&m, 5), None);
    }

    #[test]
    fn plan_pane_targets_the_named_pane_only() {
        let m = manifest(vec![(0, vec![pane(10, true, Z), pane(2, false, None), pane(3, false, None)])]);
        let pc = plan_close(CloseScope::Pane, &m, 0, Some(3), |id| id == 3, 9).unwrap();
        assert_eq!(pc.targets, vec![3]);
        assert_eq!(pc.awaiting, BTreeSet::from([3]));
    }

    #[test]
    fn plan_tab_targets_all_terminals_and_awaits_claude_ones() {
        let m = manifest(vec![(0, vec![pane(10, true, Z), pane(2, false, None), pane(3, false, None)])]);
        let pc = plan_close(CloseScope::Tab, &m, 0, None, |id| id == 2, 0).unwrap();
        assert_eq!(pc.targets, vec![2, 3]);
        assert_eq!(pc.awaiting, BTreeSet::from([2]));
    }

    #[test]
    fn plan_is_none_for_unknown_tab_or_missing_pane() {
        let m = manifest(vec![(0, vec![pane(10, true, Z)])]);
        assert!(plan_close(CloseScope::Tab, &m, 4, None, |_| true, 0).is_none());
        assert!(plan_close(CloseScope::Pane, &m, 0, None, |_| true, 0).is_none());
        assert!(plan_close(CloseScope::Tab, &m, 0, None, |_| true, 0).is_none());
    }
}
