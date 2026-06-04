#!/usr/bin/env bash
# zellaude-hook.sh — Claude Code hook → zellij pipe bridge
# Forwards hook events to the zellaude Zellij plugin via pipe.
#
# Usage in ~/.claude/settings.json hooks:
#   "command": "/path/to/zellaude-hook.sh"

# Ensure zellij is on PATH — cargo install puts it in ~/.cargo/bin which is
# not inherited by Claude Code hook processes spawned as bash subprocesses
export PATH="$HOME/.cargo/bin:$PATH"

# Exit silently if not running inside Zellij
[ -z "$ZELLIJ_SESSION_NAME" ] && exit 0
[ -z "$ZELLIJ_PANE_ID" ] && exit 0

# Run a command with a hard wall-clock limit so a stuck `zellij pipe` (server
# busy / socket never answers) can never accumulate. Without this, each event
# leaves an orphaned `zellij pipe` blocked in unix_stream_data_wait, holding a
# server connection thread; thousands pile up and peg the box. Prefers GNU
# `timeout` (Linux) or `gtimeout` (macOS coreutils); falls back to a watchdog.
run_bounded() {
  local secs=$1; shift
  if command -v timeout >/dev/null 2>&1; then
    timeout -k 1 "$secs" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout -k 1 "$secs" "$@"
  else
    "$@" &
    local pid=$!
    ( sleep "$secs"; kill -9 "$pid" 2>/dev/null ) &
    local wd=$!
    wait "$pid" 2>/dev/null
    kill "$wd" 2>/dev/null
  fi
}

# Single jq pass: parse stdin once and emit a tab-separated line of
# hook_event, tool_name (for the shell branches below) and the compact
# payload JSON. Previously this forked jq 6× per event — on a busy session
# that flood of short-lived processes hammers per-exec monitors (opensnitch)
# and the scheduler. ts_ms (event ordering, ms granularity) is taken inside
# the same jq via `now`; the cost vs capturing pre-read is sub-millisecond.
INPUT=$(cat)
# Field order matters: PAYLOAD (always non-empty, no tabs) and HOOK_EVENT come
# first; TOOL_NAME is last because it can be empty and tab is IFS-whitespace,
# so a trailing empty field collapses to an empty TOOL_NAME instead of shifting.
IFS=$'\t' read -r PAYLOAD HOOK_EVENT TOOL_NAME < <(
  printf '%s' "$INPUT" | jq -rc \
    --arg pane_id "$ZELLIJ_PANE_ID" \
    --arg zellij_session "$ZELLIJ_SESSION_NAME" \
    --arg term_program "${TERM_PROGRAM:-}" \
    '
    (.hook_event_name // "") as $ev |
    (.tool_name // "") as $tool |
    [ ({
        pane_id: ($pane_id | tonumber),
        session_id: (.session_id // ""),
        hook_event: $ev,
        tool_name: (if $tool == "" then null else $tool end),
        cwd: (.cwd // null),
        zellij_session: $zellij_session,
        term_program: (if $term_program == "" then null else $term_program end),
        ts_ms: (now * 1000 | floor)
      } | @json),
      $ev,
      $tool
    ] | @tsv'
)

[ -z "$HOOK_EVENT" ] && exit 0

# Permission request: bell + desktop notification
if [ "$HOOK_EVENT" = "PermissionRequest" ]; then
  printf '\a' > /dev/tty 2>/dev/null || true

  # Read notification setting (default: Always)
  SETTINGS_FILE="$HOME/.config/zellij/plugins/zellaude.json"
  NOTIFY_MODE="Always"
  if [ -f "$SETTINGS_FILE" ]; then
    NOTIFY_MODE=$(jq -r '.notifications // "Always"' "$SETTINGS_FILE" 2>/dev/null)
  fi

  # For "Unfocused" mode, check if the terminal app is frontmost
  SHOULD_NOTIFY=false
  case "$NOTIFY_MODE" in
    Always) SHOULD_NOTIFY=true ;;
    Unfocused)
      TERM_FOCUSED=false
      case "$(uname)" in
        Darwin)
          # Map TERM_PROGRAM to macOS process name
          EXPECTED="${TERM_PROGRAM:-}"
          case "$EXPECTED" in
            Apple_Terminal) EXPECTED="Terminal" ;;
            iTerm.app)     EXPECTED="iTerm2" ;;
          esac
          FRONT_APP=$(osascript -e 'tell application "System Events" to get name of first application process whose frontmost is true' 2>/dev/null)
          [ "$FRONT_APP" = "$EXPECTED" ] && TERM_FOCUSED=true
          ;;
        Linux)
          # X11: check if focused window belongs to our terminal
          if command -v xdotool >/dev/null 2>&1; then
            ACTIVE_PID=$(xdotool getactivewindow getwindowpid 2>/dev/null)
            if [ -n "$ACTIVE_PID" ]; then
              # Walk up the process tree from our shell to see if the
              # focused window's process is an ancestor (i.e. our terminal)
              PID=$$
              while [ "$PID" -gt 1 ] 2>/dev/null; do
                [ "$PID" = "$ACTIVE_PID" ] && { TERM_FOCUSED=true; break; }
                PID=$(ps -o ppid= -p "$PID" 2>/dev/null | tr -d ' ')
              done
            fi
          fi
          # Wayland: no standard way to check; fall through to not-focused
          ;;
      esac
      [ "$TERM_FOCUSED" = false ] && SHOULD_NOTIFY=true
      ;;
  esac

  if [ "$SHOULD_NOTIFY" = true ]; then
    TOOL_SUFFIX=""
    [ -n "$TOOL_NAME" ] && TOOL_SUFFIX=" — $TOOL_NAME"
    TITLE="⚠ Claude Code"
    MESSAGE="Permission requested${TOOL_SUFFIX}"

    # Rate-limit: one notification per pane per 10 seconds
    LOCK="/tmp/zellaude-notify-${ZELLIJ_PANE_ID}"
    NOW=$(date +%s)
    LAST=0
    [ -f "$LOCK" ] && LAST=$(cat "$LOCK" 2>/dev/null)
    if [ $((NOW - LAST)) -ge 10 ]; then
      echo "$NOW" > "$LOCK"

      # Click callback: activate terminal + focus the pane
      ZELLIJ_BIN=$(command -v zellij)
      FOCUS_CMD="${ZELLIJ_BIN} -s '${ZELLIJ_SESSION_NAME}' pipe --name zellaude:focus -- ${ZELLIJ_PANE_ID}"

      case "$(uname)" in
        Darwin)
          [ -n "${TERM_PROGRAM:-}" ] && FOCUS_CMD="open -a '${TERM_PROGRAM}' && ${FOCUS_CMD}"
          if command -v terminal-notifier >/dev/null 2>&1; then
            terminal-notifier \
              -title "$TITLE" \
              -message "$MESSAGE" \
              -group "zellaude-permission-${ZELLIJ_PANE_ID}" \
              -execute "$FOCUS_CMD" &
          else
            osascript -e "display notification \"$MESSAGE\" with title \"$TITLE\"" &
          fi
          ;;
        Linux)
          if command -v notify-send >/dev/null 2>&1; then
            notify-send "$TITLE" "$MESSAGE" &
          fi
          ;;
      esac
    fi
  fi
fi

# Dismiss the pending permission notification once the user has acted on it.
# These events all imply the in-terminal prompt is gone:
#   PostToolUse / PostToolUseFailure — the tool ran or was blocked (accept/deny)
#   UserPromptSubmit                 — the user typed a new message
#   Stop / SessionEnd                — end of turn / session (safety net)
# PreToolUse is intentionally excluded: it fires before the prompt is shown.
case "$HOOK_EVENT" in
  PostToolUse|PostToolUseFailure|UserPromptSubmit|Stop|SessionEnd)
    case "$(uname)" in
      Darwin)
        if command -v terminal-notifier >/dev/null 2>&1; then
          terminal-notifier -remove "zellaude-permission-${ZELLIJ_PANE_ID}" >/dev/null 2>&1
        fi
        ;;
      # Linux: notify-send offers no group-remove; left for a follow-up.
    esac
    # Reset the rate-limit so the next request can notify without the 10s delay.
    rm -f "/tmp/zellaude-notify-${ZELLIJ_PANE_ID}" 2>/dev/null || true
    ;;
esac

# Send to plugin with a hard timeout — never block/accumulate if the server
# is slow or the pipe goes unanswered (see run_bounded above).
run_bounded 3 zellij pipe --name "zellaude" -- "$PAYLOAD"
