# taarof / VTE — OSC 7 directory + git branch reporting
# Source this from ~/.zshrc on local and remote machines.
# Reports hostname + CWD to the terminal emulator and encodes the current git
# branch in the terminal title before every prompt.
#
# Usage:
#   echo 'source /path/to/osc7.zsh' >> ~/.zshrc
#
# Or copy the function into your existing zsh config.

autoload -Uz add-zsh-hook

__taarof_osc7() {
  # URL-encode the path (spaces, unicode, etc.)
  local LC_ALL=C
  local encoded_path=""
  local c i
  for (( i=1; i<=${#PWD}; i++ )); do
    c="${PWD[i]}"
    case "$c" in
      [a-zA-Z0-9/_.-]) encoded_path+="$c" ;;
      *) encoded_path+=$(printf '%%%02X' "'$c") ;;
    esac
  done
  printf '\e]7;file://%s%s\e\\' "${HOST:-${HOSTNAME}}" "$encoded_path"
}

__taarof_git_branch() {
  git symbolic-ref --quiet --short HEAD 2>/dev/null \
    || git rev-parse --short HEAD 2>/dev/null
}

__taarof_title() {
  local host="${HOST:-${HOSTNAME}}"
  local title="${USER}@${host}:${PWD}"
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

add-zsh-hook precmd __taarof_prompt_state

# Report initial state on shell startup
__taarof_prompt_state
