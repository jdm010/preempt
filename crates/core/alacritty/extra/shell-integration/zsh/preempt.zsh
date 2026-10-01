[[ -o interactive ]] || return

function _preempt_clear_prediction {
    builtin printf '\033]777;AT;0\007' > /dev/tty
}

function _preempt_publish_prediction {
    emulate -L zsh

    if (( CURSOR != ${#BUFFER} )); then
        _preempt_clear_prediction
        return
    fi

    if [[ $BUFFER == *[[:cntrl:]]* ]]; then
        _preempt_clear_prediction
        return
    fi

    local encoded=$BUFFER
    encoded=${encoded//\%/%25}
    encoded=${encoded//;/%3B}
    encoded=${encoded//$'\e'/%1B}
    encoded=${encoded//$'\a'/%07}
    encoded=${encoded//$'\r'/%0D}
    encoded=${encoded//$'\n'/%0A}

    # Cap characters so UTF-8 payloads stay below VTE's 1024-byte OSC limit.
    if (( ${#encoded} > 200 )); then
        _preempt_clear_prediction
        return
    fi

    builtin printf '\033]777;AT;1;%s\007' "$encoded" > /dev/tty
}

autoload -Uz add-zle-hook-widget
add-zle-hook-widget line-pre-redraw _preempt_publish_prediction
add-zle-hook-widget line-finish _preempt_clear_prediction
