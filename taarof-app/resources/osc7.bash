# taarof / VTE — OSC 7 directory + git branch reporting for bash
# Source this from ~/.bashrc on local and remote machines.
#
# Usage:
#   echo 'source /path/to/osc7.bash' >> ~/.bashrc

__taarof_osc7() {
  printf '\e]7;file://%s%s\e\\' "${HOSTNAME}" "${PWD}"
}

__taarof_git_branch() {
  git symbolic-ref --quiet --short HEAD 2>/dev/null \
    || git rev-parse --short HEAD 2>/dev/null
}

__taarof_title() {
  local title="${USER}@${HOSTNAME}:${PWD}"
  local branch
  branch="$(__taarof_git_branch)"
  if [[ -n "$branch" ]]; then
    title="${title} [taarof-git:${branch}]"
  fi
  printf '\e]2;%s\a' "$title"
}

__taarof_prompt_state() {
  __taarof_osc7
  __taarof_title
}

PROMPT_COMMAND="__taarof_prompt_state${PROMPT_COMMAND:+;${PROMPT_COMMAND}}"
