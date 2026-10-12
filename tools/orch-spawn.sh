#!/usr/bin/env bash
# orch-spawn.sh — создать окно orchestrator отдельно от atlas (тот же план,
# что у кнопки O: `wezterm cli spawn --new-window -- omp -c` + заголовок
# вкладки atlas-orchestrator, чтобы W/O-кнопки atlas находили именно её).
#
# Проверка «уже есть» hardened, как в orch-group: панель ищется по
# `wezterm cli list --format json` (tab_title: точное совпадение → подстрока;
# затем прежняя мягкая проверка по title), а tab_title виден и у неактивной
# вкладки — поэтому второй pane из-за неактивной вкладки не создаётся.
# Найденная панель дополнительно сверяется с окнами Hyprland
# (hyprctl clients -j), если hyprctl доступен.
#
#   (по умолчанию)  dry-run — печатает, что будет сделано; если панель уже
#                            есть, честно сообщает её pane-id
#   --apply                 — выполнить: новое окно wezterm с omp -c
#   --cwd P                 — каталог оркестратора (по умолчанию ~/Projects/atlas)
#   --title S               — заголовок вкладки (по умолчанию atlas-orchestrator)
#
# Флаги --dry-run и --apply взаимоисключающие: оба сразу или повтор любого —
# предупреждение на stderr и выход 2. Без флагов — dry-run.
#
# Существующие панели не трогает: если orchestrator уже найден — exit 0.

set -euo pipefail

APPLY=0
CWD="$HOME/Projects/atlas"
TITLE="atlas-orchestrator"

usage() {
	awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
	exit 0
}

n_dry=0 n_apply=0
while [[ $# -gt 0 ]]; do
	case "$1" in
	--apply) n_apply=$((n_apply + 1)) ;;
	--dry-run) n_dry=$((n_dry + 1)) ;;
	--cwd) CWD="${2:?--cwd требует аргумент}"; shift ;;
	--title) TITLE="${2:?--title требует аргумент}"; shift ;;
	-h | --help) usage ;;
	*) echo "orch-spawn: неизвестный аргумент: $1" >&2; exit 1 ;;
	esac
	shift
done

if ((n_dry + n_apply > 1)); then
	echo "orch-spawn: --dry-run и --apply взаимоисключающие (повторы тоже запрещены)" >&2
	echo "использование: orch-spawn [--dry-run|--apply] [--cwd P] [--title S]" >&2
	exit 2
fi
((n_apply == 1)) && APPLY=1

command -v wezterm >/dev/null || { echo "orch-spawn: нет wezterm" >&2; exit 2; }
[[ -d "$CWD" ]] || { echo "orch-spawn: нет каталога $CWD" >&2; exit 2; }

panes_json="$(wezterm cli list --format json 2>/dev/null)" || panes_json=""

if [[ -z "$panes_json" ]]; then
	# CLI wezterm недоступен. Если окно с таким заголовком видно в Hyprland —
	# orchestrator уже есть и спавнить нечего; иначе дальше всё равно тупик.
	if command -v hyprctl >/dev/null 2>&1 &&
		clients_json="$(hyprctl clients -j 2>/dev/null)" &&
		jq -e --arg t "$TITLE" '.[] | select(.class == "org.wezfurlong.wezterm") | select(.title | contains($t))' >/dev/null 2>&1 <<<"$clients_json"; then
		echo "orchestrator уже есть (окно wezterm с заголовком «$TITLE» в Hyprland), но wezterm cli list недоступен — spawn не выполнялся"
		exit 0
	fi
	echo "orch-spawn: wezterm cli list недоступен — не могу проверить существующие панели" >&2
	exit 2
fi

# приоритет: точный tab_title → подстрока tab_title → прежняя мягкая проверка
# по содержимому «orchestrator» (чтобы точно не создать вторую панель)
existing="$(jq -r --arg t "$TITLE" '
	( [ .[] | select(.tab_title == $t) ]
	  + [ .[] | select((.tab_title // "") != $t and ((.tab_title // "") | contains($t))) ]
	  + [ .[] | select(((.tab_title // "") + " " + (.title // "") | ascii_downcase) | contains("orchestrator")) ] )
	| .[0]
	| if . == null then empty
	  else [ (.pane_id|tostring), (.tab_id|tostring), (.tab_title // ""), (.title // "") ] | @tsv end
' <<<"$panes_json" 2>/dev/null)" || existing=""

if [[ -n "$existing" ]]; then
	hypr_note=""
	if command -v hyprctl >/dev/null 2>&1; then
		clients_json="$(hyprctl clients -j 2>/dev/null)" || clients_json=""
		if [[ -n "$clients_json" ]]; then
			hypr_row="$(jq -r --arg t "$TITLE" '
				[.[] | select(.class == "org.wezfurlong.wezterm" and (.title | contains($t)))][0]
				| if . == null then empty else [.address, .title] | @tsv end' <<<"$clients_json" 2>/dev/null)" || hypr_row=""
			if [[ -n "$hypr_row" ]]; then
				hypr_note="; Hyprland: $(cut -f1 <<<"$hypr_row") «$(cut -f2 <<<"$hypr_row")»"
			else
				# панель есть, но заголовок окна её не показывает — вкладка неактивна
				hypr_note="; в Hyprland окно с заголовком «$TITLE» не видно (вкладка не активна в своём окне wezterm), но панель существует"
			fi
		fi
	fi
	echo "orchestrator уже есть: pane $(cut -f1 <<<"$existing"), tab $(cut -f2 <<<"$existing") «$(cut -f3 <<<"$existing")»$hypr_note — spawn не нужен"
	exit 0
fi

echo "--- план ($( [[ $APPLY == 1 ]] && echo apply || echo dry-run )) ---"
echo "  \$ wezterm cli spawn --new-window --cwd $CWD -- omp -c"
echo "  \$ wezterm cli set-tab-title --tab-id <tab-id> $TITLE   # tab-id берётся из списка после spawn"

if [[ $APPLY != 1 ]]; then
	echo "dry-run: ничего не выполнено. Запустить: $0 --apply"
	exit 0
fi

pane_id="$(wezterm cli spawn --new-window --cwd "$CWD" -- omp -c)" || {
	echo "orch-spawn: wezterm cli spawn завершился с ошибкой" >&2
	exit 5
}
pane_id="$(tr -d '[:space:]' <<<"$pane_id")"
[[ "$pane_id" =~ ^[0-9]+$ ]] || { echo "orch-spawn: spawn вернул неожиданное: «$pane_id»" >&2; exit 5; }

tab_id=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
	tab_id="$(wezterm cli list --format json | jq -r --argjson p "$pane_id" '.[] | select(.pane_id == $p) | .tab_id' | head -1)"
	[[ -n "$tab_id" ]] && break
	sleep 0.5
done
[[ -n "$tab_id" ]] || { echo "orch-spawn: панель $pane_id создана, но tab-id не найден — заголовок не установлен" >&2; exit 5; }

wezterm cli set-tab-title --tab-id "$tab_id" "$TITLE" >/dev/null
echo "готово: pane $pane_id в окне orchestrator, tab $tab_id → «$TITLE»"
