# Source from config.fish; reports CWD before every prompt, including over SSH.
if not functions -q __taarof_prompt_state
    function __taarof_prompt_state --on-event fish_prompt
        set -l encoded (string escape --style=url -- "$PWD" | string replace -a '%2F' '/')
        printf '\e]7;file://%s%s\e\\' (hostname) "$encoded"
    end
end
