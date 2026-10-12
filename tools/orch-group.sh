#!/usr/bin/env bash
# orch-group.sh — «цельное окно» для оркестратора: складывает окно atlas и
# окно orchestrator (wezterm, вкладка atlas-orchestrator) в одну
# Hyprland-группу — контейнер-вкладки в одном кадре, как i3 "tabbed".
#
# Как ищется orchestrator (по порядку):
#   1. `wezterm cli list --format json`: панель, у которой tab_title равен
#      atlas-orchestrator (затем — содержит; крайний случай — подстрока в
#      title панели). Это надёжно: tab_title виден даже у неактивной вкладки.
#   2. Если панель есть, но ни одно wezterm-окно Hyprland не называется
#      «atlas-orchestrator» — вкладка не активна в своём окне (заголовок
#      wezterm-окна показывает только активную вкладку). Скрипт активирует
#      панель: `wezterm cli activate-pane --pane-id <id>` (запасной путь —
#      `activate-tab --tab-id <id>`) и до 1,5 с опрашивает hyprctl clients -j,
#      пока заголовок окна не обновится. Это делается и в dry-run: без
#      активации окно просто не найти.
#   3. Если такой панели в wezterm нет — подсказка (кнопка O / spawn) и
#      выход 3. Если CLI wezterm недоступен — поиск по заголовку Hyprland,
#      как раньше.
#
# Режимы:
#   (по умолчанию)   group   — dry-run: только печатает план dispatch-команд
#   --apply                  — выполнить план (фокус мигнёт — это ожидаемо)
#   --ungroup                — обратная операция: вынуть orchestrator из группы
#                              и распустить группу из двух окон
#   --ungroup --apply        — выполнить ungroup
#
# Флаги --dry-run и --apply взаимоисключающие: оба сразу или повтор любого —
# предупреждение на stderr и выход 2. То же для повтора --ungroup.
# Без флагов — dry-run.
#
# Опции: --class C (класс окна atlas, по умолчанию "atlas"),
#        --orch-title S (подстрока заголовка окна/вкладки orchestrator,
#        по умолчанию "atlas-orchestrator"), -h/--help.
#
# Скрипт трогает ТОЛЬКО окна, найденные по классу/заголовку: atlas (class) и
# wezterm-окно с вкладкой atlas-orchestrator. Всё остальное — отказ.
#
# Коды выхода: 0 — успех или уже сделано; 1 — неизвестный аргумент;
# 2 — конфликт флагов или нет hyprctl/jq; 3 — окна не найдены/делать нечего;
# 4 — прочие отказы (разные воркспейсы, перекрытие окон); 5 — post-apply
# проверка не прошла; 6 — отказ трогать чужую/существующую группу.
#
# Проверено на Hyprland 0.56.2: dispatchers togglegroup / moveintogroup /
# moveoutofgroup / focuswindow присутствуют в установленном бинарнике;
# семантика по wiki 0.54: togglegroup — «toggles the current active window
# into a group», moveintogroup — «Moves the active window into a group in a
# specified direction. No-op if there is no group in the specified
# direction», moveoutofgroup — «Moves the active window out of a group…
# or window for a specific window».

set -euo pipefail

MODE="group"
APPLY=0
ATLAS_CLASS="atlas"
ORCH_TITLE="atlas-orchestrator"

usage() {
	awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
	exit 0
}

n_dry=0 n_apply=0 n_ungroup=0
while [[ $# -gt 0 ]]; do
	case "$1" in
	--apply) n_apply=$((n_apply + 1)) ;;
	--dry-run) n_dry=$((n_dry + 1)) ;;
	--ungroup) n_ungroup=$((n_ungroup + 1)) ;;
	--class) ATLAS_CLASS="${2:?--class требует аргумент}"; shift ;;
	--orch-title) ORCH_TITLE="${2:?--orch-title требует аргумент}"; shift ;;
	-h | --help) usage ;;
	*) echo "orch-group: неизвестный аргумент: $1 (см. --help)" >&2; exit 1 ;;
	esac
	shift
done

if ((n_dry + n_apply > 1)) || ((n_ungroup > 1)); then
	echo "orch-group: --dry-run и --apply взаимоисключающие (повторы тоже запрещены); --ungroup указывается один раз" >&2
	echo "использование: orch-group [--dry-run|--apply] [--ungroup] [--class C] [--orch-title S]" >&2
	exit 2
fi
((n_apply == 1)) && APPLY=1
((n_ungroup == 1)) && MODE="ungroup"

for bin in hyprctl jq; do
	command -v "$bin" >/dev/null || { echo "orch-group: нет $bin" >&2; exit 2; }
done

die() { echo "orch-group: $1" >&2; exit "${2:-4}"; }

# ---------------------------------------------------------------- состояние --

CLIENTS="$(hyprctl clients -j)"

# Перечитать hyprctl clients и пересобрать все выборки.
snapshot() {
	CLIENTS="$(hyprctl clients -j)"

	# atlas-окна: class == ATLAS_CLASS. Выбор: то же воркспейс, что и
	# orchestrator, иначе сфокусированное, иначе с меньшим focusHistoryID
	# (свежее в фокусе).
	mapfile -t ATLAS_ROWS < <(jq -r --arg c "$ATLAS_CLASS" '
		.[] | select(.class == $c)
		| [.address, (.workspace.id|tostring), (.grouped|tostring),
		   (.focusHistoryID|tostring), (.focused == true), .title] | @tsv' <<<"$CLIENTS" || true)

	# wezterm-окна Hyprland, чей заголовок содержит ORCH_TITLE
	ORCH_ROWS="$(jq -r --arg t "$ORCH_TITLE" '
		.[] | select(.class == "org.wezfurlong.wezterm")
		| select(.title | contains($t))
		| [.address, (.workspace.id|tostring), (.grouped|tostring),
		   (.focusHistoryID|tostring), (.focused == true), .title] | @tsv' <<<"$CLIENTS")"

	ORCH_ROW="$(sort -t$'\t' -k4,4n <<<"$ORCH_ROWS" | head -1 || true)"
	ORCH_COUNT="$(grep -c . <<<"$ORCH_ROWS" || true)"
	return 0
}

snapshot

# --------------------------------------------- orchestrator: панель wezterm --

# Сначала ищем панель в wezterm: tab_title виден и у неактивной вкладки,
# в отличие от заголовка окна Hyprland.
PANE_TSV=""
if command -v wezterm >/dev/null 2>&1; then
	panes_json="$(wezterm cli list --format json 2>/dev/null)" || panes_json=""
	if [[ -n "$panes_json" ]]; then
		# приоритет: точный tab_title → подстрока tab_title → подстрока title
		PANE_TSV="$(jq -r --arg t "$ORCH_TITLE" '
			( [ .[] | select(.tab_title == $t) ]
			  + [ .[] | select((.tab_title // "") != $t and ((.tab_title // "") | contains($t))) ]
			  + [ .[] | select(((.tab_title // "") | contains($t)) | not)
			      | select((.title // "") | contains($t)) ] )
			| .[0]
			| if . == null then empty
			  else [ (.pane_id|tostring), (.tab_id|tostring),
			         (.tab_title // ""), (.title // "") ] | @tsv end
		' <<<"$panes_json" 2>/dev/null)" || PANE_TSV=""
	else
		echo "orch-group: предупреждение: wezterm cli list недоступен — ищу только по заголовку Hyprland" >&2
	fi
fi

pane_id="" tab_id=""
if [[ -n "$PANE_TSV" ]]; then
	pane_id="$(cut -f1 <<<"$PANE_TSV")"
	tab_id="$(cut -f2 <<<"$PANE_TSV")"
	if [[ -z "$ORCH_ROW" ]]; then
		# Панель есть, но заголовок её wezterm-окна в Hyprland не содержит
		# ORCH_TITLE: вкладка orchestrator не активна. Активируем панель и
		# коротко опрашиваем hyprctl, пока заголовок окна не обновится.
		echo "панель orchestrator найдена в wezterm (pane $pane_id, tab $tab_id), но её вкладка не активна в своём окне"
		echo "  \$ wezterm cli activate-pane --pane-id $pane_id"
		if ! wezterm cli activate-pane --pane-id "$pane_id" >/dev/null 2>&1; then
			echo "  \$ wezterm cli activate-tab --tab-id $tab_id"
			wezterm cli activate-tab --tab-id "$tab_id" >/dev/null 2>&1 || true
		fi
		for _ in 1 2 3 4 5; do # ≤1,5 с: 5 × 0,3 с
			sleep 0.3
			snapshot
			if [[ -n "$ORCH_ROW" ]]; then break; fi
		done
	fi
fi

if [[ -z "$ORCH_ROW" ]]; then
	if [[ -n "$PANE_TSV" ]]; then
		die "вкладка orchestrator есть в wezterm (pane $pane_id), но после activate-pane заголовок «$ORCH_TITLE» у окон Hyprland так и не появился" 3
	fi
	die "окно orchestrator не найдено (класс org.wezfurlong.wezterm, вкладка/заголовок содержит «$ORCH_TITLE»).
Откройте его кнопкой O в atlas, либо:
  wezterm cli spawn --new-window --cwd ~/Projects/atlas -- omp -c
  # затем: wezterm cli set-tab-title --tab-id <id> atlas-orchestrator" 3
fi

row_field() { # $1=row, $2=index (tab-separated TSV)
	awk -v n="$2" '{print $n}' <<<"$1"
}

pick_atlas() { # $1 = orch workspace id или ""
	local best="" best_score=99
	local row ws focused score
	for row in "${ATLAS_ROWS[@]}"; do
		[[ -n "$row" ]] || continue
		ws="$(row_field "$row" 2)"
		focused="$(row_field "$row" 5)"
		# приоритет: тот же воркспейс, что orchestrator (иначе группировка
		# невозможна), затем сфокусированное atlas-окно, затем первое
		if [[ -n "${1:-}" && "$ws" == "$1" ]]; then score=0
		elif [[ "$focused" == "true" ]]; then score=1
		else score=2; fi
		if [[ -z "$best" || "$score" -lt "$best_score" ]]; then
			best="$row"; best_score="$score"
		fi
	done
	[[ -n "$best" ]] || return 1
	printf '%s' "$best"
}

# ------------------------------------------------------------------- group ---

do_group() {
	local orch_addr orch_ws orch_grouped orch_title
	orch_addr="$(row_field "$ORCH_ROW" 1)"
	orch_ws="$(row_field "$ORCH_ROW" 2)"
	orch_grouped="$(row_field "$ORCH_ROW" 3)"
	orch_title="$(cut -f6- <<<"$ORCH_ROW")"

	local atlas_row atlas_addr atlas_ws atlas_grouped atlas_title
	atlas_row="$(pick_atlas "$orch_ws")" || die "не найдено ни одного окна класса «$ATLAS_CLASS»" 3
	atlas_addr="$(row_field "$atlas_row" 1)"
	atlas_ws="$(row_field "$atlas_row" 2)"
	atlas_grouped="$(row_field "$atlas_row" 3)"
	atlas_title="$(cut -f6- <<<"$atlas_row")"

	echo "atlas:       $atlas_addr «$atlas_title» (ws $atlas_ws)"
	echo "orchestrator: $orch_addr «$orch_title» (ws $orch_ws, из $ORCH_COUNT совпадений)"

	if [[ "$atlas_grouped" != "[]" ]]; then
		if jq -e --arg a "$orch_addr" 'index($a)' >/dev/null 2>&1 <<<"$atlas_grouped"; then
			echo "уже сгруппированы: $atlas_grouped — делать нечего"
			exit 0
		fi
		die "atlas-окно уже в другой группе ($atlas_grouped) — не трогаю чужую группу" 6
	fi
	[[ "$orch_grouped" == "[]" ]] || die "окно orchestrator уже в другой группе ($orch_grouped) — сначала --ungroup" 6
	[[ "$atlas_ws" == "$orch_ws" ]] || die "окна на разных воркспейсах (atlas: $atlas_ws, orchestrator: $orch_ws).
Сначала перенесите orchestrator:
  hyprctl dispatch movetoworkspacesilent \"${atlas_ws},address:${orch_addr}\""

	# направление от orchestrator к atlas по доминирующей оси (центры)
	local dx dy adx ady dir
	dx=$(jq -n --argjson ax "$(jq -r '.[] | select(.address=="'"$atlas_addr"'") | .at[0] + .size[0]/2' <<<"$CLIENTS")" \
		--argjson ox "$(jq -r '.[] | select(.address=="'"$orch_addr"'") | .at[0] + .size[0]/2' <<<"$CLIENTS")" \
		'$ax - $ox')
	dy=$(jq -n --argjson ay "$(jq -r '.[] | select(.address=="'"$atlas_addr"'") | .at[1] + .size[1]/2' <<<"$CLIENTS")" \
		--argjson oy "$(jq -r '.[] | select(.address=="'"$orch_addr"'") | .at[1] + .size[1]/2' <<<"$CLIENTS")" \
		'$ay - $oy')
	adx=$((dx < 0 ? -dx : dx)); ady=$((dy < 0 ? -dy : dy))
	if ((adx <= 2 && ady <= 2)); then
		die "окна перекрываются (dx=$dx dy=$dy) — направление группировки не определено"
	elif ((adx > ady)); then
		if ((dx > 0)); then dir=r; else dir=l; fi
	else
		if ((dy > 0)); then dir=d; else dir=u; fi
	fi
	echo "направление: $dir (dx=$dx, dy=$dy)"

	local -a PLAN=(
		"hyprctl dispatch focuswindow address:$atlas_addr"
		"hyprctl dispatch togglegroup"
		"hyprctl dispatch focuswindow address:$orch_addr"
		"hyprctl dispatch moveintogroup $dir"
	)

	echo "--- план ($( [[ $APPLY == 1 ]] && echo apply || echo dry-run )) ---"
	printf '  $ %s\n' "${PLAN[@]}"

	if [[ $APPLY != 1 ]]; then
		echo "dry-run: ничего не выполнено. Запустить: $0 --apply"
		return 0
	fi

	local cmd
	for cmd in "${PLAN[@]}"; do
		$cmd >/dev/null
	done
	sleep 0.3
	snapshot
	local now="$CLIENTS"
	if jq -e --arg a "$atlas_addr" '.[] | select(.address==$a) | (.grouped | index("'"$orch_addr"'"))' >/dev/null <<<"$now"; then
		echo "готово: orchestrator в группе atlas-окна ($atlas_addr). Переключение вкладок группы: hyprctl dispatch changegroupactive f|b"
	else
		echo "внимание: после apply orchestrator не виден в grouped у $atlas_addr — проверьте hyprctl clients -j" >&2
		return 5
	fi
}

# ----------------------------------------------------------------- ungroup ---

do_ungroup() {
	[[ -n "$ORCH_ROW" ]] || die "окно orchestrator не найдено — снимать нечего" 3
	local orch_addr orch_grouped orch_title
	orch_addr="$(row_field "$ORCH_ROW" 1)"
	orch_grouped="$(row_field "$ORCH_ROW" 3)"
	orch_title="$(cut -f6- <<<"$ORCH_ROW")"

	[[ "$orch_grouped" != "[]" ]] || die "окно orchestrator не в группе" 3

	# atlas-окно из той же группы
	local atlas_addr="" row gaddr
	for row in "${ATLAS_ROWS[@]}"; do
		[[ -n "$row" ]] || continue
		for gaddr in $(jq -r '.[]' <<<"$(row_field "$row" 3)"); do
			if [[ "$gaddr" == "$orch_addr" ]]; then atlas_addr="$(row_field "$row" 1)"; break 2; fi
		done
	done
	[[ -n "$atlas_addr" ]] || die "в группе orchestrator ($orch_grouped) нет atlas-окна класса «$ATLAS_CLASS» — посторонняя группа, не трогаю" 6

	local others
	others="$(jq -r --arg o "$orch_addr" --arg a "$atlas_addr" '[.[] | select(. != $o and . != $a)] | length' <<<"$orch_grouped")"

	echo "orchestrator: $orch_addr «$orch_title»"
	echo "atlas:        $atlas_addr; прочих участников группы: $others"

	local -a PLAN=("hyprctl dispatch moveoutofgroup address:$orch_addr")
	if [[ "$others" == "0" ]]; then
		PLAN+=("hyprctl dispatch focuswindow address:$atlas_addr" "hyprctl dispatch togglegroup")
	else
		echo "в группе есть посторонние окна ($((others))) — группу не распускаю, только вынимаю orchestrator"
	fi

	echo "--- план ($( [[ $APPLY == 1 ]] && echo apply || echo dry-run )) ---"
	printf '  $ %s\n' "${PLAN[@]}"

	if [[ $APPLY != 1 ]]; then
		echo "dry-run: ничего не выполнено. Запустить: $0 --ungroup --apply"
		return 0
	fi

	local cmd
	for cmd in "${PLAN[@]}"; do
		$cmd >/dev/null
	done
	sleep 0.3
	snapshot
	local now="$CLIENTS"
	local still
	still="$(jq -r --arg a "$orch_addr" '.[] | select(.address==$a) | (.grouped|length)' <<<"$now")"
	if [[ "$still" == "0" ]]; then
		echo "готово: orchestrator вне группы"
	else
		echo "внимание: orchestrator всё ещё в группе ($still участников) — проверьте hyprctl clients -j" >&2
		return 5
	fi
}

case "$MODE" in
group) do_group ;;
ungroup) do_ungroup ;;
esac
