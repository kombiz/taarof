# taarof shell integration — OSC 133 command markers (SAFE, idempotent).
#
# Sourcing this file makes your shell emit standard and VTE prompt signals so
# taarof can:
#   * copy exactly the previous command's output (Ctrl+Shift+O) — no prompt
#     lines and no 200-line truncation, and
#   * jump between prompts (Ctrl+Shift+Up / Ctrl+Shift+Down).
#
# Emit standard OSC 133 for other terminals and VTE's explicit prompt signal
# (OSC 666;vte.shell.precmd! ST), as supplied by VTE's own vte.sh integration.
# Taarof observes that valueless termprop to record the prompt cursor row.
# OSC 133 alone does not raise this termprop on supported VTE releases.
# Both sequences work over SSH because the local VTE parses the byte stream.
#
# Supports bash and zsh (auto-detected). Idempotent: a sentinel guard makes a
# second `source` a no-op, so it is safe to add to both an rc file and a
# per-host profile.
#
# Usage — add to ~/.bashrc or ~/.zshrc (adjust the path to wherever you keep it,
# e.g. copy it to ~/.config/taarof/ alongside osc7.bash / osc7.zsh):
#     source "$HOME/.config/taarof/taarof-shell-integration.sh"
#
# Fish must source taarof-shell-integration.fish instead.
# Bash uses PS0 for command-start C (Bash 4.4+), preserving existing DEBUG traps
# and PS0 content. Zsh uses preexec. Taarof's prompt-row tracking uses the VTE
# signal; OSC 133 A/C/D remain standard interoperability metadata.

# Idempotency guard: a second `source` in the same shell is a no-op. `return`
# works at the top level of a sourced file; the `|| exit 0` covers the harmless
# case of this file being run instead of sourced.
if [ -n "${__TAAROF_OSC133:-}" ]; then
  return 0 2>/dev/null || exit 0
fi
__TAAROF_OSC133=1

# Emit VTE's prompt signal plus standard A after reporting exit status D. `\033` (ESC) is used instead
# of `\e` for portability, terminated by ST (ESC backslash).
__taarof_osc133_precmd() {
  printf '\033]133;D;%s\033\\' "$?"
  printf '\033]666;vte.shell.precmd!\033\\'
  printf '\033]133;A\033\\'
}

# Emit the command-start marker (C) just before a command runs.
__taarof_osc133_preexec() {
  printf '\033]133;C\033\\'
}

if [ -n "${ZSH_VERSION:-}" ]; then
  autoload -Uz add-zsh-hook 2>/dev/null
  # add-zsh-hook is inherently deduped, so re-registration is impossible.
  add-zsh-hook precmd __taarof_osc133_precmd
  add-zsh-hook preexec __taarof_osc133_preexec
elif [ -n "${BASH_VERSION:-}" ]; then
  # Preserve both Bash scalar and array PROMPT_COMMAND forms. The sentinel
  # above prevents duplicate registration. Avoid replacing an existing DEBUG
  # trap; PS0 supplies C below without changing user trap behavior.
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a "* ]]; then
    PROMPT_COMMAND=(__taarof_osc133_precmd "${PROMPT_COMMAND[@]}")
  else
    PROMPT_COMMAND="__taarof_osc133_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  fi
  # PS0 runs before each interactive command without replacing a DEBUG trap.
  # Keep existing PS0 content after the standard command-start marker.
  PS0=$'\033]133;C\033\\'"${PS0:-}"
fi
