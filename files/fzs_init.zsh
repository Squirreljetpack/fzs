
export fzs_name="{{ fzs_name }}"
export FZS_ROOT_DIR="{{ fzs_root_dir }}"
export FZS_PATH_DIR="{{ fzs_path_dir }}"
export FZS_DATA_DIR="{{ fzs_data_dir }}"
export FZS_CONFIG_DIR="{{ fzs_config_dir }}"
export fzs_provides_file="{{ fzs_provides_file }}"
export fzs_fzf_dir_cmd="{{ fzs_fzf_dir_cmd }}"
export fzs_fzf_pager_cmd="{{ fzs_fzf_pager_cmd }}"
export fzs_init_file="{{ fzs_init_file }}"
export fzs_plugins_file="{{ fzs_plugins_file }}"

$fzs_name._cleanup-prompt.wg() {
  [[ $# -ge 1 ]] && BUFFER="$1"
  CURSOR=${2:-#BUFFER}
  zle redisplay
}

$fzs_name._in () {
  local d="${3:-,}"
  [[ "$d$1$d" == *"$d$2$d"* ]];
}

$fzs_name._base-select.wg () {
    fzf \
      --delimiter '\t' \
      --with-nth '1,4..' \
      --preview '{{ fzs_fzf_base_preview }}' \
      --layout=reverse \
      --height=70% \
      --bind 'space:accept' \
      --bind 'one:transform:[[ {q} != ^ ]] && echo accept' \
      --bind "zero:transform-query(printf \"%s\" \"\$FZF_QUERY\" | sed \"s/^\^//\")" \
      --query '^' "${@}"
}

$fzs_name.plugin-select.wg () {
  INIT_BUFFER="$BUFFER"
  local fn_table="{{ fn_table }}"
  selected=$(
    "{{ fzs_name }}"._base-select.wg \
     --preview "echo {2}; {{ fzs_fzf_dir_cmd }} {2}" \
    <<< "$fn_table"
  )
  [[ -z "$selected" ]] && "{{ fzs_name }}".cleanup-prompt-widget && return
  zle reset-prompt

  IFS=$'\t' read -r alias dir cmd rest <<<"$selected"
  zle $cmd
}
zle -N $fzs_name.plugin-select.wg

$fzs_name.all-fn-select.wg () {
  INIT_BUFFER="$BUFFER"
  local fn_table="{{ all_fn_table }}"
  selected=$(
    "{{ fzs_name }}"._base-select.wg <<< "$fn_table"
  )
  [[ -z "$selected" ]] && "{{ fzs_name }}"._cleanup-prompt.wg && return
  zle reset-prompt

  IFS=$'\t' read -r pg_alias cmd rest <<<"$selected"

  LBUFFER+="$cmd "
}
zle -N $fzs_name.all-fn-select.wg
