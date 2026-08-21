# Source this from ~/.bashrc or ~/.zshrc after deploying taarof-agent-wrapper.
# These wrappers are intended for CLIs without native structured hook events.
# Leave Claude unwrapped when you have ~/.claude hook support enabled.

alias codex='taarof-agent-wrapper codex'
alias aider='taarof-agent-wrapper aider'
alias opencode='taarof-agent-wrapper opencode'
