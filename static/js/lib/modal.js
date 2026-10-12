/* static/js/lib/modal.js — самописные модалки: askDialog/confirmDialog.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { el, esc } from './dom.js';

function askDialog(titleText, fields) {
  return new Promise((resolve) => {
    const ov = el('div', 'overlay');
    const dlg = el('div', 'dlg out');
    dlg.appendChild(el('h3', null, esc(titleText)));
    const inputs = [];
    for (const f of fields) {
      const row = el('div', 'drow');
      row.appendChild(el('label', null, esc(f.label)));
      let inp;
      if (f.multiline) {
        inp = el('textarea', 'fld');
        inp.rows = 4;
      } else {
        inp = el('input', 'fld');
        inp.type = 'text';
      }
      inp.spellcheck = false;
      inp.autocomplete = 'off';
      if (f.value != null) inp.value = f.value;
      row.appendChild(inp);
      dlg.appendChild(row);
      inputs.push(inp);
    }
    const btns = el('div', 'dbtns');
    const ok = el('button', 'btn95', 'ОК');
    const cancel = el('button', 'btn95', 'Отмена');
    btns.append(ok, cancel);
    dlg.appendChild(btns);
    ov.appendChild(dlg);
    document.body.appendChild(ov);
    inputs[0].focus();
    const done = (val) => { ov.remove(); document.removeEventListener('keydown', key, true); resolve(val); };
    const submit = () => done(inputs.map((i) => i.value.trim()));
    const key = (e) => {
      if (e.key === 'Escape') { e.stopPropagation(); done(null); }
      else if (e.key === 'Enter' && e.target && e.target.tagName === 'TEXTAREA') {
        /* в многострочном поле Enter переносит строку, отправка — Ctrl+Enter */
        if (e.ctrlKey) { e.preventDefault(); e.stopPropagation(); submit(); }
      }
      else if (e.key === 'Enter') { e.stopPropagation(); submit(); }
    };
    document.addEventListener('keydown', key, true);
    ok.addEventListener('click', submit);
    cancel.addEventListener('click', () => done(null));
    ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(null); });
  });
}

/* однострочное подтверждение (тот же вид диалога, что askDialog): ОК/Отмена */
function confirmDialog(titleText) {
  return new Promise((resolve) => {
    const ov = el('div', 'overlay');
    const dlg = el('div', 'dlg out');
    dlg.appendChild(el('h3', null, esc(titleText)));
    const btns = el('div', 'dbtns');
    const ok = el('button', 'btn95', 'ОК');
    const cancel = el('button', 'btn95', 'Отмена');
    btns.append(ok, cancel);
    dlg.appendChild(btns);
    ov.appendChild(dlg);
    document.body.appendChild(ov);
    ok.focus();
    const done = (v) => { ov.remove(); document.removeEventListener('keydown', key, true); resolve(v); };
    const key = (e) => {
      if (e.key === 'Escape') { e.stopPropagation(); done(false); }
      else if (e.key === 'Enter') { e.stopPropagation(); done(true); }
    };
    document.addEventListener('keydown', key, true);
    ok.addEventListener('click', () => done(true));
    cancel.addEventListener('click', () => done(false));
    ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(false); });
  });
}

export { askDialog, confirmDialog };
