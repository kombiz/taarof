# Source from config.fish; native Fish OSC 133 prompt and command markers.
# Function registration is idempotent and preserves existing prompt handlers.
if not functions -q __taarof_osc133_prompt
    function __taarof_osc133_prompt --on-event fish_prompt
        set -l previous_status $status
        printf '\e]133;D;%s\e\\\e]666;vte.shell.precmd!\e\\\e]133;A\e\\' $previous_status
    end
    function __taarof_osc133_preexec --on-event fish_preexec
        printf '\e]133;C\e\\'
    end
end
