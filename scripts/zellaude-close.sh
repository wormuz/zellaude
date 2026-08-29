#!/usr/bin/env bash
[ -z "$ZELLIJ_SESSION_NAME" ] && exit 0
[ -z "$ZELLIJ_PANE_ID" ] && exit 0

SCOPE="pane"
[ "${1:-}" = "tab" ] && SCOPE="tab"

if command -v timeout >/dev/null 2>&1; then
  timeout 5 zellij pipe --name "zellaude:close" -- "$SCOPE $ZELLIJ_PANE_ID" || true
else
  zellij pipe --name "zellaude:close" -- "$SCOPE $ZELLIJ_PANE_ID" || true
fi
