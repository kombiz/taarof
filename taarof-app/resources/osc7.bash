# taarof / VTE — OSC 7 directory + git branch reporting for bash
# Source this from ~/.bashrc on local and remote machines.
#
# Usage:
#   echo 'source /path/to/osc7.bash' >> ~/.bashrc

__taarof_osc7() {
  local LC_ALL=C encoded_path="" c i
  for (( i=0; i<${#PWD}; i++ )); do
    c="${PWD:i:1}"
    case "$c" in
      [a-zA-Z0-9/_.-]) encoded_path+="$c" ;;
      *) printf -v c '%%%02X' "'$c"; encoded_path+="$c" ;;
    esac
  done
  printf '\e]7;file://%s%s\e\\' "${HOSTNAME}" "$encoded_path"
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

# Preserve scalar and array prompt hooks, and make repeated source idempotent.
if [[ ${__TAAROF_OSC7_BASH:-} != 1 ]]; then
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a "* ]]; then
    PROMPT_COMMAND=(__taarof_prompt_state "${PROMPT_COMMAND[@]}")
  else
    PROMPT_COMMAND="__taarof_prompt_state${PROMPT_COMMAND:+;${PROMPT_COMMAND}}"
  fi
  __TAAROF_OSC7_BASH=1
fi
