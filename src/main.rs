mod close;
mod event_handler;
#[cfg(not(test))]
mod installer;
mod palette;
mod render;
mod rename_guard;
mod state;
mod tab_pane_map;

use palette::{Palette, ThemeSource};
use state::{
    unix_now, unix_now_ms, HookPayload, MenuAction, PendingRename, SessionInfo, Settings, State,
    ViewMode,
};
use std::collections::{BTreeMap, HashMap};
use close::{CloseRequest, CloseScope};
use zellij_tile::prelude::*;

const DONE_TIMEOUT: u64 = 30;
const TIMER_INTERVAL: f64 = 1.0;
const FLASH_TICK: f64 = 0.25;
const MAX_TAB_TITLE: usize = 40;
/// Minimum gap between tab-rename batches. See `apply_tab_titles`.
const RENAME_COOLDOWN_MS: u64 = 400;
/// How many times a single desired rename is re-issued before being abandoned.
const RENAME_MAX_ATTEMPTS: u8 = 3;

/// Strip control characters and clamp length before using a pane title as a tab
/// name. The status bar truncates further for display; this just bounds it.
///
/// Leading non-alphanumeric glyphs are dropped: Claude Code prefixes its OSC
/// title with an animated spinner (✳ ◐ ◑ ◒ …) that changes every frame. Keeping
/// it would make the sanitized title differ on each frame → rename_tab →
/// TabUpdate → full tab-bar redraw, i.e. the visible flicker while Claude works.
fn sanitize_tab_title(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .skip_while(|c| !c.is_alphanumeric())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_TAB_TITLE)
        .collect()
}
const CLOSE_GRACE_MS: u64 = 10_000;

register_plugin!(State);

#[cfg(test)]
mod tests {
    use super::sanitize_tab_title;

    #[test]
    fn spinner_frames_sanitize_to_same_title() {
        for spinner in ["✳", "◐", "◑", "◒", "·"] {
            assert_eq!(
                sanitize_tab_title(&format!("{spinner} Fix flicker")),
                "Fix flicker"
            );
        }
        assert_eq!(sanitize_tab_title("◑ Діагностика миготіння"), "Діагностика миготіння");
        assert_eq!(sanitize_tab_title("✳✶✻"), "");
    }
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
            PermissionType::ReadCliPipes,
            PermissionType::MessageAndLaunchOtherPlugins,
            PermissionType::WriteToStdin,
        ]);
        self.own_plugin_id = get_plugin_ids().plugin_id;
        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::ModeUpdate,
            EventType::Timer,
            EventType::Mouse,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
        set_timeout(TIMER_INTERVAL);

        // Parse color configuration from the KDL plugin block and resolve the
        // initial palette (theme overlay is applied later on the first
        // ModeUpdate, then re-resolved).
        let (theme_source, overrides) = palette::parse_config(&configuration);
        self.theme_source = theme_source;
        self.palette_overrides = overrides;
        self.recompute_palette();

        // Load persisted settings (may be retried in PermissionRequestResult
        // if this fires before permissions are granted)
        self.load_config();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::TabUpdate(tabs) => {
                let new_active = tabs.iter().find(|t| t.active).map(|t| t.position);
                if new_active != self.active_tab_index {
                    // Tab focus changed — clear persist flashes on the newly focused tab
                    if let Some(idx) = new_active {
                        self.clear_flashes_on_tab(idx);
                    }
                }
                self.active_tab_index = new_active;
                self.tabs = tabs;
                self.rebuild_pane_map();
                true
            }
            Event::PaneUpdate(manifest) => {
                self.pane_manifest = Some(manifest);
                self.rebuild_pane_map();
                if let Some(pc) = &mut self.pending_close {
                    pc.awaiting.retain(|id| self.pane_to_tab.contains_key(id));
                }
                self.settle_close();
                true
            }
            Event::ModeUpdate(mode_info) => {
                self.input_mode = mode_info.mode;
                if let Some(name) = mode_info.session_name {
                    self.zellij_session_name = Some(name);
                }
                self.host_styling = Some(mode_info.style.colors);
                self.recompute_palette();
                true
            }
            Event::Mouse(Mouse::LeftClick(_, col)) => {
                let col = col as usize;

                // Check prefix click region first → toggle ViewMode
                if let Some((start, end)) = self.prefix_click_region {
                    if col >= start && col < end {
                        self.view_mode = match self.view_mode {
                            ViewMode::Normal => ViewMode::Settings,
                            ViewMode::Settings => ViewMode::Normal,
                        };
                        return true;
                    }
                }

                match self.view_mode {
                    ViewMode::Normal => {
                        // Overflow arrows take priority over tab click regions.
                        for nav in &self.nav_arrows {
                            if col >= nav.start_col && col < nav.end_col {
                                match nav.direction {
                                    state::NavDirection::Left => {
                                        self.tab_scroll_offset =
                                            self.tab_scroll_offset.saturating_sub(1);
                                    }
                                    state::NavDirection::Right => {
                                        self.tab_scroll_offset += 1;
                                    }
                                }
                                return true;
                            }
                        }
                        for region in &self.click_regions {
                            if col >= region.start_col && col < region.end_col {
                                if region.is_waiting {
                                    focus_terminal_pane(region.pane_id, false, false);
                                } else {
                                    switch_tab_to(region.tab_index as u32 + 1);
                                }
                                return false;
                            }
                        }
                        false
                    }
                    ViewMode::Settings => {
                        let action = self
                            .menu_click_regions
                            .iter()
                            .find(|r| col >= r.start_col && col < r.end_col)
                            .map(|r| r.action);
                        match action {
                            Some(MenuAction::ToggleSetting(key)) => {
                                match key {
                                    state::SettingKey::Notifications => {
                                        self.settings.notifications =
                                            self.settings.notifications.cycle();
                                    }
                                    state::SettingKey::Flash => {
                                        self.settings.flash = self.settings.flash.cycle();
                                    }
                                    state::SettingKey::ElapsedTime => {
                                        self.settings.elapsed_time = !self.settings.elapsed_time;
                                    }
                                    state::SettingKey::ModeIndicator => {
                                        self.settings.mode_indicator = !self.settings.mode_indicator;
                                    }
                                    state::SettingKey::TabTitles => {
                                        self.settings.tab_titles = !self.settings.tab_titles;
                                    }
                                }
                                self.save_config();
                                // Apply immediately so the toggle takes effect without
                                // waiting for the next pane/tab update (no-op when off).
                                self.apply_tab_titles();
                                true
                            }
                            Some(MenuAction::CloseMenu) => {
                                self.view_mode = ViewMode::Normal;
                                true
                            }
                            None => false,
                        }
                    }
                }
            }
            Event::RunCommandResult(exit_code, stdout, _stderr, context) => {
                match context.get("type").map(|s| s.as_str()) {
                    Some("load_config") if exit_code == Some(0) => {
                        let raw = String::from_utf8_lossy(&stdout);
                        if let Ok(settings) = serde_json::from_str::<Settings>(raw.trim()) {
                            self.settings = settings;
                        }
                        self.config_loaded = true;
                        true
                    }
                    Some("install_hooks") => {
                        self.hooks_installed = true;
                        false
                    }
                    _ => false,
                }
            }
            Event::Timer(_) => {
                if self
                    .pending_close
                    .as_ref()
                    .map_or(false, |pc| unix_now_ms() >= pc.deadline_ms)
                {
                    self.finish_close();
                }
                let stale_changed = self.cleanup_stale_sessions();
                let flash_changed = self.cleanup_expired_flashes();
                let has_flashes = self.has_active_flashes();
                if has_flashes {
                    set_timeout(FLASH_TICK);
                } else {
                    set_timeout(TIMER_INTERVAL);
                }
                has_flashes || stale_changed || flash_changed || self.has_elapsed_display()
            }
            Event::PermissionRequestResult(_) => {
                // Now that permissions are granted, mark as non-selectable
                // so the plugin stays visible during fullscreen
                set_selectable(false);
                // Permissions granted — ask existing instances for their state
                self.request_sync();
                // Retry config load (the one in load() may have been dropped
                // because it ran before permissions were granted)
                if !self.config_loaded {
                    self.load_config();
                }
                // Auto-install hook script and register Claude Code hooks
                #[cfg(not(test))]
                if !self.hooks_installed {
                    installer::run_install();
                }
                false
            }
            _ => false,
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        match pipe_message.name.as_str() {
            "zellaude" => {
                // Hook event from CLI
                let payload_str = match pipe_message.payload {
                    Some(ref s) => s,
                    None => return false,
                };
                let payload: HookPayload = match serde_json::from_str(payload_str) {
                    Ok(p) => p,
                    Err(_) => return false,
                };
                event_handler::handle_hook_event(self, payload);
                true
            }
            "zellaude:focus" => {
                // Notification click — focus the requested pane
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(pane_id) = payload.trim().parse::<u32>() {
                        focus_terminal_pane(pane_id, false, false);
                    }
                }
                false
            }
            "zellaude:close" => {
                let req = CloseRequest::parse(pipe_message.payload.as_deref());
                self.start_close(req);
                false
            }
            "zellaude:exec-close" => {
                if let Some(ref p) = pipe_message.payload {
                    if let Ok(tab) = p.trim().parse::<usize>() {
                        self.exec_tab_close(tab);
                    }
                }
                false
            }
            "zellaude:request" => {
                // Another instance asking for state — respond with ours
                self.broadcast_sessions();
                false
            }
            "zellaude:settings" => {
                // Another instance broadcast new settings
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(settings) = serde_json::from_str::<Settings>(payload) {
                        self.settings = settings;
                        return true;
                    }
                }
                false
            }
            "zellaude:sync" => {
                // Another instance sharing state — merge it
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(sessions) =
                        serde_json::from_str::<BTreeMap<u32, SessionInfo>>(payload)
                    {
                        self.merge_sessions(sessions);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        render::render_status_bar(self, rows, cols);
    }
}

impl State {
    /// Rebuild the resolved palette: built-in defaults, then the Zellij theme
    /// (if selected), then explicit overrides.
    pub fn recompute_palette(&mut self) {
        let mut p = Palette::default();
        if self.theme_source == ThemeSource::Zellij {
            if let Some(styling) = self.host_styling {
                palette::apply_theme(&mut p, &styling);
            }
        }
        palette::apply_overrides(&mut p, &self.palette_overrides);
        self.palette = p;
    }

    fn rebuild_pane_map(&mut self) {
        if let Some(ref manifest) = self.pane_manifest {
            self.pane_to_tab = tab_pane_map::build_pane_to_tab_map(&self.tabs, manifest);
            self.refresh_session_tab_names();
            self.remove_dead_panes();
            self.apply_tab_titles();
        }
    }

    /// When enabled, name each Claude tab after its pane's title (the OSC title
    /// the program set). Unopinionated: the label content is whatever set the
    /// pane title; zellaude only mirrors it onto the tab.
    ///
    /// # Why this is defensive
    ///
    /// `rename_tab` addresses a tab by 1-based *position*, and zellij has no
    /// stable per-tab identifier to use instead. Upstream bug
    /// `zellij-org/zellij#3535` makes the server's idea of tab positions drift
    /// from the plugin's after tabs are closed, so a rename can land on the
    /// wrong tab or on no tab at all ("Failed to find tab with index"). A
    /// rename that never lands leaves `tab.name` unchanged, which the naive
    /// "rename whenever desired != current" rule reads as "still needs
    /// renaming" — re-issuing it on every `TabUpdate`/`PaneUpdate`. Since each
    /// rename attempt itself provokes a `TabUpdate`, that is a feedback loop.
    ///
    /// Three independent brakes, so no single failure can spin:
    ///
    /// 1. **Stale-position guard.** A rename is only issued when the plugin's
    ///    view of the tab is internally consistent (the position indexes back
    ///    to the same tab). This is what stops writes landing on the wrong tab.
    /// 2. **Bounded retries.** Each desired rename is tracked as a
    ///    `PendingRename`; if it does not land within `RENAME_MAX_ATTEMPTS`, it
    ///    is abandoned rather than retried forever.
    /// 3. **Rate limit.** At most one rename batch per `RENAME_COOLDOWN_MS`.
    ///
    /// Worst case under this scheme is a tab that keeps its old label, never a
    /// crash. The brakes stay while `#3535` is open upstream.
    fn apply_tab_titles(&mut self) {
        if !self.settings.tab_titles {
            self.pending_renames.clear();
            return;
        }
        let titles = match self.pane_manifest {
            Some(ref manifest) => tab_pane_map::build_pane_titles(manifest),
            None => return,
        };

        // Brake 3: rate limit. Renames are cosmetic, so dropping a batch costs
        // nothing but a slightly stale label until the next update.
        let now_ms = unix_now_ms();
        if now_ms.saturating_sub(self.last_rename_ms) < RENAME_COOLDOWN_MS {
            return;
        }

        // Pick the most recently active Claude session per tab as the title source.
        let mut best: HashMap<usize, &SessionInfo> = HashMap::new();
        for session in self.sessions.values() {
            if let Some(idx) = session.tab_index {
                let replace = best
                    .get(&idx)
                    .map_or(true, |cur| session.last_event_ts >= cur.last_event_ts);
                if replace {
                    best.insert(idx, session);
                }
            }
        }

        // Retire pending renames that have landed, or whose tab is gone. Keyed
        // by the pre-rename name, which is what we can still match on.
        let live_names: std::collections::HashSet<&str> =
            self.tabs.iter().map(|t| t.name.as_str()).collect();
        self.pending_renames.retain(|observed_name, pending| {
            // Landed: the desired name is now present in the tab list.
            if live_names.contains(pending.desired.as_str()) {
                return false;
            }
            // Tab vanished (closed, or renamed by someone else) — stop tracking.
            if !live_names.contains(observed_name.as_str()) {
                return false;
            }
            true
        });

        let mut renames: Vec<(u32, String, String)> = Vec::new();
        for (index_in_list, tab) in self.tabs.iter().enumerate() {
            // Brake 1: stale-position guard.
            if !rename_guard::position_is_addressable(tab.position, index_in_list) {
                continue;
            }

            let Some(session) = best.get(&tab.position) else {
                continue;
            };
            let Some(title) = titles.get(&session.pane_id) else {
                continue;
            };
            let desired = sanitize_tab_title(title);
            if desired.is_empty() || desired == tab.name {
                continue;
            }

            // Brake 2: bounded retries. Once exhausted, leave the tab with its
            // current name rather than re-issuing forever.
            let prior = self.pending_renames.get(&tab.name);
            if rename_guard::rename_budget_exhausted(
                prior.map(|p| p.desired.as_str()),
                &desired,
                prior.map_or(0, |p| p.attempts),
                RENAME_MAX_ATTEMPTS,
            ) {
                continue;
            }

            renames.push((tab.position as u32 + 1, desired, tab.name.clone()));
        }

        if renames.is_empty() {
            return;
        }
        self.last_rename_ms = now_ms;

        for (tab_position, desired, observed_name) in renames {
            let entry = self
                .pending_renames
                .entry(observed_name)
                .or_insert_with(|| PendingRename {
                    desired: desired.clone(),
                    attempts: 0,
                });
            if entry.desired != desired {
                // Target changed since the last attempt — restart the budget.
                entry.desired = desired.clone();
                entry.attempts = 0;
            }
            entry.attempts = entry.attempts.saturating_add(1);

            rename_tab(tab_position, desired);
        }
    }

    fn refresh_session_tab_names(&mut self) {
        for session in self.sessions.values_mut() {
            if let Some((idx, name)) = self.pane_to_tab.get(&session.pane_id) {
                session.tab_index = Some(*idx);
                session.tab_name = Some(name.clone());
            }
        }
    }

    fn remove_dead_panes(&mut self) {
        self.sessions
            .retain(|pane_id, _| self.pane_to_tab.contains_key(pane_id));
    }

    fn cleanup_stale_sessions(&mut self) -> bool {
        let now = unix_now();
        let mut changed = false;
        for session in self.sessions.values_mut() {
            match session.activity {
                state::Activity::Done | state::Activity::AgentDone => {
                    if now.saturating_sub(session.last_event_ts) >= DONE_TIMEOUT {
                        session.activity = state::Activity::Idle;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        changed
    }

    fn clear_flashes_on_tab(&mut self, tab_idx: usize) {
        let pane_ids: Vec<u32> = self
            .sessions
            .values()
            .filter(|s| s.tab_index == Some(tab_idx))
            .map(|s| s.pane_id)
            .collect();
        for pane_id in pane_ids {
            self.flash_deadlines.remove(&pane_id);
        }
    }

    fn has_active_flashes(&self) -> bool {
        let now = unix_now_ms();
        self.flash_deadlines.values().any(|&deadline| now < deadline)
    }

    fn cleanup_expired_flashes(&mut self) -> bool {
        let before = self.flash_deadlines.len();
        let now = unix_now_ms();
        self.flash_deadlines.retain(|_, deadline| now < *deadline);
        self.flash_deadlines.len() != before
    }

    fn has_elapsed_display(&self) -> bool {
        if !self.settings.elapsed_time {
            return false;
        }
        let now = unix_now();
        self.sessions.values().any(|s| {
            !matches!(s.activity, state::Activity::Idle)
                && now.saturating_sub(s.last_event_ts) >= DONE_TIMEOUT
        })
    }

    fn request_sync(&self) {
        pipe_message_to_plugin(MessageToPlugin::new("zellaude:request"));
    }

    fn broadcast_sessions(&self) {
        let mut msg = MessageToPlugin::new("zellaude:sync");
        msg.message_payload =
            Some(serde_json::to_string(&self.sessions).unwrap_or_default());
        pipe_message_to_plugin(msg);
    }

    fn broadcast_settings(&self) {
        let mut msg = MessageToPlugin::new("zellaude:settings");
        msg.message_payload =
            Some(serde_json::to_string(&self.settings).unwrap_or_default());
        pipe_message_to_plugin(msg);
    }

    fn load_config(&self) {
        let mut ctx = BTreeMap::new();
        ctx.insert("type".into(), "load_config".into());
        run_command(
            &[
                "sh",
                "-c",
                "cat \"$HOME/.config/zellij/plugins/zellaude.json\" 2>/dev/null || echo '{}'",
            ],
            ctx,
        );
    }

    fn save_config(&self) {
        if !self.config_loaded {
            return;
        }
        self.broadcast_settings();
        let json = serde_json::to_string(&self.settings).unwrap_or_default();
        let json_esc = json.replace('\'', "'\\''");
        let cmd = format!(
            "mkdir -p \"$HOME/.config/zellij/plugins\" && printf '%s' '{json_esc}' > \"$HOME/.config/zellij/plugins/zellaude.json\""
        );
        let mut ctx = BTreeMap::new();
        ctx.insert("type".into(), "save_config".into());
        run_command(&["sh", "-c", &cmd], ctx);
    }

    fn start_close(&mut self, req: CloseRequest) {
        if self.pending_close.is_some() {
            return;
        }
        let Some(manifest) = &self.pane_manifest else {
            return;
        };
        let Some(target_pane) = req.pane_id else {
            return;
        };
        let Some(target_tab) = close::tab_of_terminal(manifest, target_pane) else {
            return;
        };
        if close::brain(manifest, target_tab) != Some(self.own_plugin_id) {
            return;
        }
        let Some(pc) = close::plan_close(
            req.scope,
            manifest,
            target_tab,
            Some(target_pane),
            |id| self.sessions.contains_key(&id),
            unix_now_ms() + CLOSE_GRACE_MS,
        ) else {
            return;
        };
        for &id in &pc.awaiting {
            write_chars_to_pane_id("/exit\r", PaneId::Terminal(id));
        }
        self.pending_close = Some(pc);
        self.settle_close();
    }

    fn exec_tab_close(&mut self, tab: usize) {
        if let Some(manifest) = &self.pane_manifest {
            if close::actor(manifest, CloseScope::Tab, tab) == Some(self.own_plugin_id) {
                close_tab_with_index(tab);
            }
        }
    }

    pub fn note_session_end(&mut self, pane_id: u32) {
        if let Some(pc) = &mut self.pending_close {
            pc.awaiting.remove(&pane_id);
        }
        self.settle_close();
    }

    fn settle_close(&mut self) {
        if self
            .pending_close
            .as_ref()
            .map_or(false, |pc| pc.awaiting.is_empty())
        {
            self.finish_close();
        }
    }

    fn finish_close(&mut self) {
        let Some(pc) = self.pending_close.take() else {
            return;
        };
        match pc.scope {
            CloseScope::Pane => {
                for id in pc.targets {
                    if self.pane_to_tab.contains_key(&id) {
                        close_terminal_pane(id);
                    }
                }
            }
            CloseScope::Tab => {
                let hand = self
                    .pane_manifest
                    .as_ref()
                    .and_then(|m| close::actor(m, CloseScope::Tab, pc.tab_index));
                if hand == Some(self.own_plugin_id) {
                    close_tab_with_index(pc.tab_index);
                } else {
                    let mut msg = MessageToPlugin::new("zellaude:exec-close");
                    msg.message_payload = Some(pc.tab_index.to_string());
                    pipe_message_to_plugin(msg);
                }
            }
        }
    }

    fn merge_sessions(&mut self, incoming: BTreeMap<u32, SessionInfo>) {
        for (pane_id, mut session) in incoming {
            let dominated = self
                .sessions
                .get(&pane_id)
                .map(|existing| session.last_event_ts > existing.last_event_ts)
                .unwrap_or(true);
            if dominated {
                // Refresh tab name from our local pane map
                if let Some((idx, name)) = self.pane_to_tab.get(&pane_id) {
                    session.tab_index = Some(*idx);
                    session.tab_name = Some(name.clone());
                }
                self.sessions.insert(pane_id, session);
            }
        }
    }
}
