# taarof / VTE — model activity termprop helpers for bash
#
# Usage:
#   source /path/to/agent-status.bash
#   taarof_agent_running codex "Editing terminal.rs"
#   taarof_agent_done codex "Done"
#   taarof_agent_idle
#
# These helpers use VTE's termprop OSC (OSC 666), not the legacy OSC 777
# notification path. taarof listens for:
#   vte.ext.taarof.agent.state   = running | done | idle
#   vte.ext.taarof.agent.text    = short status text
#   vte.ext.taarof.agent.source  = claude | codex | ...

__taarof_termprop_set() {
  local name="$1"
  local value="${2-}"
  value="${value//$'\r'/ }"
  value="${value//$'\n'/ }"
  printf '\e]666;%s=%s\e\\' "$name" "$value"
}

__taarof_termprop_reset() {
  printf '\e]666;%s\e\\' "$1"
}

taarof_agent_status() {
  local state="$1"
  local source="${2-}"
  local text="${3-}"

  case "$state" in
    running|done)
      # Write text/source first, then state, so taarof sees a complete update.
      if [[ -n "$text" ]]; then
        __taarof_termprop_set "vte.ext.taarof.agent.text" "$text"
      else
        __taarof_termprop_reset "vte.ext.taarof.agent.text"
      fi

      if [[ -n "$source" ]]; then
        __taarof_termprop_set "vte.ext.taarof.agent.source" "$source"
      else
        __taarof_termprop_reset "vte.ext.taarof.agent.source"
      fi

      __taarof_termprop_set "vte.ext.taarof.agent.state" "$state"
      ;;
    idle)
      # Clear state first so later text/source resets do not produce stale UI.
      __taarof_termprop_set "vte.ext.taarof.agent.state" "idle"
      __taarof_termprop_reset "vte.ext.taarof.agent.text"
      __taarof_termprop_reset "vte.ext.taarof.agent.source"
      ;;
    *)
      return 1
      ;;
  esac
}

taarof_agent_running() {
  taarof_agent_status running "${1-}" "${2-}"
}

taarof_agent_done() {
  taarof_agent_status done "${1-}" "${2-}"
}

taarof_agent_idle() {
  taarof_agent_status idle
}
