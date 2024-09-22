
"{{ selector_name }}" () {
  INIT_BUFFER="$BUFFER"
  pg_name="{{ pg_name }}"
  local fn_table="{{ fn_table }}"
  selected=$(
    "{{ fzs_name }}"._base-select.wg <<< "$fn_table"
  )
  [[ -z "$selected" ]] && "{{ fzs_name }}"._cleanup-prompt.wg && return
  zle reset-prompt

  IFS=$'\t' read -r name flags cmd desc <<<"$selected"

  case ",$flags," in
    *",PL,"*) zle $cmd; return $? ;; 
    *",NR,"*) LBUFFER+="$cmd "; return ;;
    *",W"*) zle $cmd;;
    *",SS,"*) ( ${(z)cmd} ) ;;
    *) eval $cmd ;;
  esac

  case ",$flags," in
    *",RP,"*) "{{ fzs_name }}"._cleanup-prompt.wg "$INIT_BUFFER"; return $? ;; 
    *",NC,"*) ;;
    *) "{{ fzs_name }}"._cleanup-prompt.wg; return $? ;;
  esac
}
zle -N "{{ selector_name }}"

