/* Revenant Part 2.0 mockup kit. Classic script: defines the global `Kit`. No modules, no dependencies.

   Icons: Lucide (https://lucide.dev), ISC License.
   Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of Feather (MIT).
   All other copyright (c) for Lucide are held by Lucide Contributors 2022.
   Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby
   granted, provided that the above copyright notice and this permission notice appear in all copies.
   THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING
   ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL,
   DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS.

   Page contract (see docs/superpowers/plans/2026-11-02-m2-p0-telas-identidade.md, Task 2):
     Kit.init({ page, screens: [{ id, name, os?, title, states: [{ key, label, render, after? }] }] })
   URL parameters: ?screen=N&state=key&theme=light|dark&long=1&w=1024&h=700 (screenshot mode: only the window, at 0,0)
                   ?manifest=1 (writes the screen/state list as JSON into <pre id="manifest"> and nothing else) */
var Kit = (function () {
  'use strict';

  /* brand:start */
  var SYMBOL = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
  <path d="M17.1 31 A38 38 0 1 0 48 12.05" stroke-width="6"/>
  <polyline points="56 4 48 12 56 20" stroke-width="6" style="stroke:var(--d, #b9822a)"/>
  <g transform="translate(50 51) scale(.58) translate(-52 -51)" stroke-width="12">
    <path d="M32 42 V18 H52 A16 16 0 0 1 52 50 H44"/>
    <path d="M32 62 V82"/>
    <path d="M58 62 L72 84" style="stroke:var(--d, #b9822a)"/>
  </g>
</svg>`;
  var SYMBOL_SMALL = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
  <path d="M19 33 A36 36 0 1 0 47 14.2" stroke-width="9"/>
  <polyline points="57 5 47 14 57 23" stroke-width="9" style="stroke:var(--d, #b9822a)"/>
  <g transform="translate(50 51) scale(.56) translate(-52 -51)" stroke-width="17">
    <path d="M32 84 V18 H52 A16 16 0 0 1 52 50 H32"/>
    <path d="M54 56 L70 84" style="stroke:var(--d, #b9822a)"/>
  </g>
</svg>`;
  /* brand:end */

  var P = {
    check: '<polyline points="20 6 9 17 4 12"/>',
    search: '<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>',
    shield: '<path d="M20 13c0 5-3.5 7.5-7.7 9a1 1 0 0 1-.6 0C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.2-2.7a1.2 1.2 0 0 1 1.6 0C14.5 3.8 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>',
    alert: '<path d="m21.7 18-8-14a2 2 0 0 0-3.5 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.7-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>',
    x: '<circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/><path d="m9 9 6 6"/>',
    info: '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>',
    play: '<polygon points="6 3 20 12 6 21 6 3"/>',
    pause: '<rect x="14" y="4" width="4" height="16" rx="1"/><rect x="6" y="4" width="4" height="16" rx="1"/>',
    cal: '<rect width="18" height="18" x="3" y="4" rx="2"/><path d="M16 2v4M8 2v4M3 10h18"/>',
    grid: '<rect width="7" height="7" x="3" y="3" rx="1"/><rect width="7" height="7" x="14" y="3" rx="1"/><rect width="7" height="7" x="14" y="14" rx="1"/><rect width="7" height="7" x="3" y="14" rx="1"/>',
    chart: '<path d="M3 3v16a2 2 0 0 0 2 2h16"/><path d="M18 17V9"/><path d="M13 17V5"/><path d="M8 17v-3"/>',
    list: '<path d="M3 12h.01"/><path d="M3 18h.01"/><path d="M3 6h.01"/><path d="M8 12h13"/><path d="M8 18h13"/><path d="M8 6h13"/>',
    folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
    dl: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/>',
    hdd: '<line x1="22" x2="2" y1="12" y2="12"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/><line x1="6" x2="6.01" y1="16" y2="16"/><line x1="10" x2="10.01" y1="16" y2="16"/>',
    sd: '<path d="M10 2v4"/><path d="M14 2v4"/><path d="M18 22H6a2 2 0 0 1-2-2V6l4-4h10a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2z"/>',
    usb: '<circle cx="10" cy="7" r="1"/><circle cx="4" cy="20" r="1"/><path d="M4.7 19.3 19 5"/><path d="m21 3-3 1 2 2Z"/><path d="M9.26 7.68 5 12l2 5"/><path d="m10 14 5 2 3.5-3.5"/><path d="m18 12 1-1 1 1-1 1Z"/>',
    lock: '<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
    refresh: '<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>',
    settings: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
    unplug: '<path d="m19 5 3-3"/><path d="m2 22 3-3"/><path d="M6.3 20.3a2.4 2.4 0 0 0 3.4 0L12 18l-6-6-2.3 2.3a2.4 2.4 0 0 0 0 3.4Z"/><path d="M7.5 13.5 10 11"/><path d="M10.5 16.5 13 14"/><path d="m12 6 6 6 2.3-2.3a2.4 2.4 0 0 0 0-3.4l-2.6-2.6a2.4 2.4 0 0 0-3.4 0Z"/>',
    clock: '<circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/>',
    image: '<rect width="18" height="18" x="3" y="3" rx="2" ry="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21"/>',
    video: '<path d="m16 13 5.223 3.482a.5.5 0 0 0 .777-.416V7.87a.5.5 0 0 0-.752-.432L16 10.5"/><rect x="2" y="6" width="14" height="12" rx="2"/>',
    'chevron-right': '<path d="m9 18 6-6-6-6"/>',
    'external-link': '<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
    copy: '<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
    'triangle-alert': '<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>',
    'circle-check': '<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>',
    power: '<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.77.04"/>'
  };

  var U = 'https://images.unsplash.com/photo-';
  var IMG = ['1552674605-db6ffd4facb5', '1461896836934-ffe607ba8211', '1476480862126-209bfaa8edc8', '1486218119243-13883505764c', '1571008887538-b36bb32f4571', '1502904550040-7534597429ae', '1452626038306-9aae5e071dd3', '1506905925346-21bda4d32df4', '1469474968028-56623f02e42e', '1441974231531-c6227db76b6e', '1519681393784-d120267933ba'];
  var BD = { ok: ['shield', 'Íntegro'], mid: ['alert', 'Parcial'], bad: ['x', 'Danificado'] };
  var NOTE_ICON = { ok: 'circle-check', mid: 'triangle-alert', bad: 'x', info: 'info' };
  var LENGTHEN_SEL = 'button, .btn, h1, h2, h3, h4, h5, label, .seg button, [data-t]';

  var K = {};
  var cfg = null;
  var cur = {};   // screen id -> current state key
  var wins = {};  // screen id -> .win element

  K.icon = function (name) {
    if (!P[name]) throw new Error('Kit.icon: unknown icon "' + name + '"');
    return '<svg class="i" viewBox="0 0 24 24" aria-hidden="true">' + P[name] + '</svg>';
  };
  K.photo = function (i, w) { return U + IMG[((i % IMG.length) + IMG.length) % IMG.length] + '?w=' + (w || 320) + '&q=60&auto=format&fit=crop'; };
  K.nf = function (n) { return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, '.'); };
  K.sz = function (mb) { return mb >= 1000 ? (mb / 1000).toFixed(1).replace('.', ',') + ' GB' : Math.round(mb) + ' MB'; };
  K.badge = function (state) {
    var b = BD[state];
    if (!b) throw new Error('Kit.badge: unknown state "' + state + '"');
    return '<span class="bd ' + state + '">' + K.icon(b[0]) + b[1] + '</span>';
  };
  K.note = function (kind, html) {
    if (!NOTE_ICON[kind]) throw new Error('Kit.note: unknown kind "' + kind + '"');
    return '<div class="note ' + kind + '">' + K.icon(NOTE_ICON[kind]) + '<span>' + html + '</span></div>';
  };
  function esc(s) { return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); }
  K.tech = function (code, html) {
    return '<details class="tech"><summary>Detalhes técnicos</summary>' + (html ? '<p>' + html + '</p>' : '') + '<code>' + esc(code) + '</code></details>';
  };
  K.logo = function (opts) {
    var small = !!(opts && opts.small);
    return (small ? SYMBOL_SMALL : SYMBOL).replace('<svg ', '<svg class="logo" role="img" aria-label="Revenant" style="' + (opts && opts.onTile ? '--d:var(--tile-gold);color:var(--tile-ink)' : '--d:var(--brand-gold)') + '" ');
  };
  // opts: { i, tag: 'RAW'|'JPG'|'VÍDEO', state: 'ok'|'mid'|'bad', sel, dur, w, nobadge }
  K.thumb = function (o) {
    o = o || {};
    var i = o.i || 0, tag = o.tag || 'RAW', video = tag === 'VÍDEO';
    var h = '<div class="th' + (o.sel ? ' sel' : '') + '" data-pick="' + i + '"><img src="' + K.photo(i, o.w) + '" alt="" onerror="this.remove()"><span class="tag">' + tag + '</span>';
    if (video) h += '<span class="play">' + K.icon('play') + '</span><span class="dur">' + (o.dur || '0:' + (20 + (i % 40))) + '</span>';
    h += '<button class="ck" data-f="' + i + '" aria-label="Selecionar">' + K.icon('check') + '</button>';
    if (!o.nobadge && o.state) h += K.badge(o.state);
    return h + '</div>';
  };

  // Adds ~30% more characters to the visible text, repeating the start of the text itself (es/en run longer than pt-BR).
  var lengthened = new WeakSet();
  K.lengthen = function (root) {
    root.querySelectorAll(LENGTHEN_SEL).forEach(function (el) {
      Array.prototype.forEach.call(el.childNodes, function (n) {
        if (n.nodeType !== 3 || lengthened.has(n)) return;
        var t = n.nodeValue, core = t.trim();
        if (!core) return;
        var words = core.split(/\s+/), extra = Math.ceil(core.length * 0.3), add = '', k = 0;
        while (add.length < extra) { add += ' ' + words[k % words.length]; k++; }
        n.nodeValue = t.replace(core, function () { return core + add; });
        lengthened.add(n);
      });
    });
  };

  var WIN_CTL = '<span class="ctl"><span><svg viewBox="0 0 12 12"><path d="M1 6h10"/></svg></span><span><svg viewBox="0 0 12 12"><rect x="1.5" y="1.5" width="9" height="9"/></svg></span><span><svg viewBox="0 0 12 12"><path d="M1 1l10 10M11 1L1 11"/></svg></span></span>';

  function isLong() { return document.documentElement.hasAttribute('data-long'); }

  function paint(s) {
    var win = wins[s.id], app = win.querySelector('.app');
    var st = s.states.filter(function (x) { return x.key === cur[s.id]; })[0] || s.states[0];
    app.innerHTML = st.render();
    if (isLong()) K.lengthen(app);
    if (st.after) st.after(app);
    var btns = win.parentNode.parentNode.querySelectorAll('.states button');
    Array.prototype.forEach.call(btns, function (b) { b.classList.toggle('on', b.dataset.state === st.key); });
  }

  function buildWin(s, w, h) {
    var win = document.createElement('div');
    win.className = 'win' + (s.os === 'win' ? ' is-win' : '');
    win.setAttribute('data-screen', String(s.id));
    win.style.setProperty('--w', w + 'px');
    win.style.setProperty('--h', h + 'px');
    win.innerHTML = s.os === 'win'
      ? '<div class="tb win-style"><b>' + esc(s.title) + '</b>' + WIN_CTL + '</div><div class="app"></div>'
      : '<div class="tb"><i></i><i></i><i></i><b>' + esc(s.title) + '</b></div><div class="app"></div>';
    wins[s.id] = win;
    return win;
  }

  function setTheme(t) {
    var r = document.documentElement;
    if (t === 'light' || t === 'dark') r.setAttribute('data-theme', t); else r.removeAttribute('data-theme');
  }
  function setLong(on) {
    var r = document.documentElement;
    if (on) r.setAttribute('data-long', ''); else r.removeAttribute('data-long');
  }
  function fit() {
    Array.prototype.forEach.call(document.querySelectorAll('.fit'), function (f) {
      var win = f.firstChild, w = win.offsetWidth, h = win.offsetHeight;
      var sc = Math.min(1, f.clientWidth / w);
      win.style.transform = 'scale(' + sc + ')';
      f.style.height = Math.round(h * sc) + 'px';
    });
  }

  K.init = function (c) {
    cfg = c;
    var manifest = c.screens.map(function (s) {
      return { page: c.page, screen: s.id, states: s.states.map(function (x) { return x.key; }) };
    });
    window.KIT_MANIFEST = manifest;
    if (!document.body) { document.addEventListener('DOMContentLoaded', function () { K.init(c); }); return; }
    var q = new URLSearchParams(location.search);
    var root = document.documentElement;

    if (q.get('manifest')) {
      var pre = document.createElement('pre');
      pre.id = 'manifest';
      pre.textContent = JSON.stringify(manifest);
      document.body.appendChild(pre);
      return;
    }

    var only = q.get('screen');
    if (only !== null) {
      var s = c.screens.filter(function (x) { return String(x.id) === only; })[0];
      if (!s) { document.body.textContent = 'unknown screen ' + only; return; }
      root.classList.add('shot');
      if (q.get('theme')) setTheme(q.get('theme'));
      if (q.get('long')) setLong(true);
      cur[s.id] = q.get('state') || s.states[0].key;
      var win = buildWin(s, +q.get('w') || 1280, +q.get('h') || 800);
      document.body.appendChild(win);
      paint(s);
      return;
    }

    var bar = document.createElement('div');
    bar.className = 'sim';
    bar.innerHTML = '<b>Simulação</b><span class="seg" id="sim-theme"><button data-theme-set="light">Claro</button><button data-theme-set="dark">Escuro</button><button data-theme-set="auto" class="on">Automático</button></span>'
      + '<label><input type="checkbox" id="sim-long"> Texto longo</label>';
    document.body.appendChild(bar);
    bar.addEventListener('click', function (e) {
      var b = e.target.closest('[data-theme-set]');
      if (!b) return;
      setTheme(b.dataset.themeSet);
      Array.prototype.forEach.call(bar.querySelectorAll('[data-theme-set]'), function (x) { x.classList.toggle('on', x === b); });
    });
    bar.querySelector('#sim-long').addEventListener('change', function () {
      setLong(this.checked);
      c.screens.forEach(paint);
    });

    c.screens.forEach(function (s) {
      cur[s.id] = s.states[0].key;
      var sec = document.createElement('section');
      sec.className = 'screen';
      sec.id = 'tela-' + s.id;
      sec.innerHTML = '<header><h2>Tela ' + s.id + ' · ' + esc(s.name) + '</h2><div class="states">'
        + s.states.map(function (x) { return '<button data-state="' + esc(x.key) + '">' + esc(x.label) + '</button>'; }).join('')
        + '</div></header><div class="fit"></div>';
      var fitEl = sec.querySelector('.fit');
      fitEl.appendChild(buildWin(s, 1280, 800));
      sec.querySelector('.states').addEventListener('click', function (e) {
        var b = e.target.closest('[data-state]');
        if (!b) return;
        cur[s.id] = b.dataset.state;
        paint(s);
      });
      document.body.appendChild(sec);
      paint(s);
    });
    window.addEventListener('resize', fit);
    fit();
  };

  return K;
})();
