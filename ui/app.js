// PathoSync screen. Talks to the Rust side through Tauri commands (see app/src/main.rs).
(() => {
  'use strict';

  const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
  const $ = (id) => document.getElementById(id);

  /** Builds an element: h('div', {class: 'x', onclick: fn}, 'text', child) — never parses HTML. */
  function h(tag, attrs, ...kids) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else if (k === 'value') el.value = v;
      else if (k === 'checked') el.checked = !!v;
      else el.setAttribute(k, v === true ? '' : v);
    }
    for (const kid of kids.flat()) {
      if (kid === null || kid === undefined || kid === false) continue;
      el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
    }
    return el;
  }
  const icon = (id) => {
    const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    const u = document.createElementNS('http://www.w3.org/2000/svg', 'use');
    u.setAttribute('href', '#' + id);
    s.append(u);
    return s;
  };

  let state = null; // settings, devices, launch options… (get_state)
  let status = null; // live levels (status)
  let draft = null; // settings being edited in Setup
  let busy = false;
  let polling = false;
  let lastSaved = null;
  let stopError = null;
  let capturingKey = false;
  const tiles = new Map(); // settings slot → tile elements
  const peakHold = new Map();
  const releasers = [];

  // ---------------------------------------------------------------- helpers
  const pct = (db) => Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  const micName = (slot, s = state.settings) => (s.mics[slot].name || '').trim() || `Mic ${slot + 1}`;
  function keyLabel(key) {
    if (!key) return '—';
    return key
      .split('+')
      .map((k) =>
        k === 'Control' ? 'Ctrl' : k === 'Super' ? 'Win' : k.replace(/^Key([A-Z])$/, '$1').replace(/^Digit(\d)$/, '$1').replace(/^Numpad/, 'Num ')
          .replace('MediaPlayPause', 'Play/Pause').replace('AudioVolumeMute', 'Mute').replace('AudioVolumeUp', 'Vol +').replace('AudioVolumeDown', 'Vol −'))
      .join('+');
  }
  function fmtTime(secs) {
    const s = Math.floor(secs);
    const hh = Math.floor(s / 3600);
    const mm = String(Math.floor((s % 3600) / 60)).padStart(2, '0');
    const ss = String(s % 60).padStart(2, '0');
    return hh ? `${hh}:${mm}:${ss}` : `${mm}:${ss}`;
  }
  function fmtDuration(secs) {
    const s = Math.round(secs);
    if (s < 60) return `${s} s`;
    return `${Math.floor(s / 60)} min ${s % 60} s`;
  }
  const baseName = (p) => String(p).split(/[\\/]/).filter(Boolean).pop() || String(p);
  const errText = (e) => (typeof e === 'string' ? e : (e && e.message) || String(e));

  // ---------------------------------------------------------------- main screen
  function saveFolder() {
    return (state.launch.folder || state.settings.save_folder || '').trim();
  }

  function renderSaveTo() {
    const f = saveFolder();
    $('saveTo').textContent = f ? `Saves to ${f}` : 'No save folder — choose one in Setup';
    $('saveTo').title = f;
  }

  function renderTiles() {
    const box = $('tiles');
    box.replaceChildren();
    tiles.clear();
    releasers.forEach((r) => r());
    releasers.length = 0;
    if (!state.active.length) {
      box.append(
        h('div', { class: 'card empty' },
          h('b', {}, 'No microphone is switched on'),
          'Open Setup to choose your microphones and their push-to-talk keys.',
          h('div', { style: 'margin-top:14px' }, h('button', { class: 'btn btn-primary', type: 'button', onclick: openSetup }, icon('i-gear'), 'Open Setup')))
      );
      return;
    }
    for (const slot of state.active) {
      const m = state.settings.mics[slot];
      const fill = h('div', { class: 'meter-fill' });
      const peak = h('div', { class: 'meter-peak' });
      const padLabel = h('b', {}, 'Hold to talk');
      const padSub = h('small', {}, m.key ? `or hold ${keyLabel(m.key)}` : 'no key set — use this button');
      const pad = h('div', { class: 'pad', role: 'button', tabindex: '0', 'aria-label': `Hold to talk on ${micName(slot)}` }, padLabel, padSub);
      const err = h('div', { class: 'tile-err', hidden: true });
      const footL = h('span', {});
      const footR = h('span', {});
      const root = h('article', { class: 'tile card' },
        h('div', { class: 'tile-head' },
          h('div', { style: 'min-width:0' }, h('div', { class: 'tile-name', title: micName(slot) }, micName(slot)), h('div', { class: 'tile-dev', title: m.device_name }, m.device_name || '')),
          h('span', { class: 'keycap', title: 'Push-to-talk key' }, keyLabel(m.key))),
        h('div', { class: 'meter', title: 'Level' }, fill, peak),
        pad, err,
        h('div', { class: 'tile-foot' }, footL, footR));
      attachHold(pad, slot);
      tiles.set(slot, { root, fill, peak, padLabel, padSub, err, footL, footR, key: m.key });
      box.append(root);
    }
  }

  /** On-screen push-to-talk: talk while the pointer (or Space/Enter) is held on the pad. */
  function attachHold(el, slot) {
    let down = false;
    const start = (e) => {
      if (down) return;
      down = true;
      if (e && e.pointerId !== undefined) {
        try { el.setPointerCapture(e.pointerId); } catch (_) { /* ignore */ }
      }
      invoke('ptt', { slot, pressed: true });
    };
    const end = () => {
      if (!down) return;
      down = false;
      invoke('ptt', { slot, pressed: false });
    };
    el.addEventListener('pointerdown', (e) => { if (e.button === 0) { e.preventDefault(); start(e); } });
    el.addEventListener('pointerup', end);
    el.addEventListener('pointercancel', end);
    el.addEventListener('lostpointercapture', end);
    el.addEventListener('keydown', (e) => { if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) { e.preventDefault(); start(); } });
    el.addEventListener('keyup', (e) => { if (e.key === ' ' || e.key === 'Enter') end(); });
    el.addEventListener('blur', end);
    el.addEventListener('contextmenu', (e) => e.preventDefault());
    releasers.push(end);
  }
  window.addEventListener('blur', () => releasers.forEach((r) => r()));

  const SIGNAL_MSG = {
    no_stream: 'Windows is not sending any sound from this mic.',
    silent: 'Windows is sending only silence from this mic (muted?).',
  };
  const SIGNAL_HELP = {
    no_stream: 'Bluetooth headset: connect it for calls (the "Headset … Hands-Free" microphone), not only for music. Then check Windows Settings → System → Sound → Input, and press Setup → Save & apply.',
    silent: 'Check Windows Settings → System → Sound → Input → this microphone: not muted, volume above 0. Check Settings → Privacy & security → Microphone → "Let desktop apps access your microphone" is On. Wired 3.5 mm mic: the plug must suit the socket (headset socket for 4-ring plugs, pink mic socket for 3-ring plugs).',
  };

  function updateTiles() {
    if (!status) return;
    status.mics.forEach((m, k) => {
      const slot = status.slots[k];
      const t = tiles.get(slot);
      if (!t) return;
      const level = pct(m.rms_db);
      const p = Math.max(pct(m.peak_db), (peakHold.get(slot) || 0) - 1.2);
      peakHold.set(slot, p);
      t.fill.style.width = level + '%';
      t.fill.style.setProperty('--full', level > 0.5 ? `${(100 / level) * 100}%` : '100%');
      t.peak.style.left = `calc(${p}% - 1px)`;
      const problem = m.error || SIGNAL_MSG[m.signal] || (!m.locked ? 'Waiting for sound from this microphone…' : '');
      t.root.classList.toggle('err', !!m.error);
      t.err.hidden = !problem;
      const help = !m.error && SIGNAL_HELP[m.signal];
      if (t.err.dataset.msg !== problem + (help || '')) {
        t.err.dataset.msg = problem + (help || '');
        t.err.replaceChildren(problem, help ? h('span', { class: 'tile-help' }, help) : '');
      }
      t.root.classList.toggle('onair', m.on_air);
      t.root.classList.toggle('held', m.held && !m.on_air);
      if (m.on_air) {
        t.padLabel.textContent = 'ON AIR';
        t.padSub.textContent = 'recording — release to stop talking';
      } else if (m.held) {
        t.padLabel.textContent = status.recording ? 'Talking' : 'Talking — not recording';
        t.padSub.textContent = status.recording ? 'starting…' : 'press Start recording to save this';
      } else {
        t.padLabel.textContent = 'Hold to talk';
        t.padSub.textContent = t.key ? `or hold ${keyLabel(t.key)}` : 'no key set — use this button';
      }
      t.footL.textContent = m.dropouts ? `${m.dropouts} dropout${m.dropouts > 1 ? 's' : ''}` : '';
      t.footR.textContent = '';
    });
  }

  function updateTop() {
    const running = status && status.running;
    const rec = !!(status && status.recording);
    const btn = $('recBtn');
    btn.disabled = busy || !running || !state.active.length;
    btn.classList.toggle('on', rec);
    if (!busy) $('recLabel').textContent = rec ? 'Stop & save' : 'Start recording';
    $('timer').textContent = fmtTime(rec ? status.recorded_secs : 0);
    $('timer').classList.toggle('live', rec);
    $('caseInput').disabled = rec || busy;

    const pill = $('statePill');
    const micProblem = running && status.mics.some((m) => m.error);
    pill.className = 'pill ' + (rec ? 'is-rec' : !running || micProblem ? 'is-bad' : 'is-ready');
    pill.textContent = rec ? '● Recording' : !running ? 'Microphones not ready' : micProblem ? 'Check microphones' : 'Ready';
    renderBanner();
  }

  function renderBanner() {
    const msgs = [];
    if (state.engine_error) msgs.push(state.engine_error);
    if (state.device_error) msgs.push(state.device_error);
    for (const w of state.key_warnings || []) msgs.push(w);
    if (status && status.writer_error) msgs.push(`Saving problem: ${status.writer_error}`);
    if (stopError) msgs.push(stopError);
    const b = $('banner');
    b.hidden = !msgs.length;
    const txt = $('bannerText');
    const key = msgs.join('\n');
    if (txt.dataset.key !== key) {
      txt.dataset.key = key;
      txt.replaceChildren(...msgs.map((m) => h('p', {}, m)));
    }
  }

  async function toggleRecording() {
    if (busy) return;
    busy = true;
    stopError = null;
    $('recBtn').disabled = true;
    try {
      if (!(status && status.recording)) {
        await invoke('start_recording', { case: $('caseInput').value });
        $('saved').hidden = true;
      } else {
        $('recLabel').textContent = 'Saving…';
        const r = await invoke('stop_recording');
        showSaved(r);
        if (state.launch.exit_after_save) {
          $('savedTitle').textContent = 'Recording saved — closing PathoSync…';
          setTimeout(() => invoke('exit_app'), 1500);
        }
      }
    } catch (e) {
      stopError = errText(e);
    } finally {
      busy = false;
      updateTop();
    }
  }

  function showSaved(r) {
    lastSaved = r;
    $('saved').hidden = false;
    const silent = r.segments.length === 0 || r.warnings.some((w) => w.includes('picked up no sound'));
    $('savedTitle').textContent = r.segments.length === 0 ? `Saved, but nothing was recorded (silent): ${baseName(r.folder)}`
      : silent ? `Saved, but part of it is silent: ${baseName(r.folder)}` : `Saved: ${baseName(r.folder)}`;
    $('saved').classList.toggle('is-silent', silent);
    const speakers = new Set(r.segments.map((s) => s.mic));
    $('savedMeta').textContent = `${fmtDuration(r.duration_s)} · ${r.segments.length} talk segment${r.segments.length === 1 ? '' : 's'}` + (speakers.size ? ` · ${[...speakers].join(', ')}` : ' · nobody pressed talk');
    const level = (l) => l.on_air_s <= 0 ? 'not used' : l.peak_db === null ? 'SILENT - no sound picked up' : `loudest ${l.peak_db.toFixed(0)} dB` + (l.peak_db < -45 ? ' (very quiet)' : '');
    $('savedFiles').replaceChildren(
      ...r.mp3.map((f, k) => h('li', {}, k === 0 ? 'All mics mixed: ' : `${r.levels[k - 1] ? r.levels[k - 1].mic : 'Mic'} only: `, h('code', {}, baseName(f)),
        k > 0 && r.levels[k - 1] ? h('span', { class: 'small' }, ` · ${fmtDuration(r.levels[k - 1].on_air_s)} on air, ${level(r.levels[k - 1])}`) : null)),
      ...r.mix.map((f) => h('li', {}, 'WAV, all mics mixed: ', h('code', {}, baseName(f)))),
      ...r.multitrack.map((f) => h('li', {}, 'WAV, one channel per mic: ', h('code', {}, baseName(f)))),
      h('li', {}, 'Who spoke when: ', h('code', {}, baseName(r.info)))
    );
    $('savedWarn').replaceChildren(...r.warnings.map((w) => h('li', {}, w)));
  }

  async function poll() {
    if (polling) return;
    polling = true;
    try {
      status = await invoke('status');
    } catch (_) {
      /* keep the last status */
    } finally {
      polling = false;
    }
    updateTiles();
    updateTop();
    if ($('setup').open && currentTab === 'check') renderCheck();
  }

  // ---------------------------------------------------------------- Setup
  let currentTab = 'mics';

  function showTab(name) {
    currentTab = name;
    document.querySelectorAll('.tab').forEach((t) => t.setAttribute('aria-selected', String(t.dataset.tab === name)));
    document.querySelectorAll('.pane').forEach((p) => (p.hidden = p.dataset.pane !== name));
    if (name === 'check') renderCheck();
  }

  async function refreshDevices() {
    try {
      const fresh = await invoke('get_state');
      state.devices = fresh.devices;
      state.device_error = fresh.device_error;
    } catch (e) { /* keep the old list */ }
    if (draft) renderMicRows();
  }

  function openSetup() {
    refreshDevices();
    draft = JSON.parse(JSON.stringify(state.settings));
    $('setupErr').textContent = '';
    $('clapMsg').textContent = '';
    renderMicRows();
    renderOptions();
    showTab('mics');
    $('setup').showModal();
  }

  function deviceById(id) {
    return state.devices.find((d) => d.id === id);
  }

  function renderMicRows() {
    $('micRows').replaceChildren(...draft.mics.map((m, i) => micRow(m, i)));
  }

  function micRow(m, i) {
    const dev = deviceById(m.device_id);
    const devSelect = h('select', {
      onchange: (e) => {
        const d = deviceById(e.target.value);
        m.device_id = d ? d.id : '';
        m.device_name = d ? d.name : '';
        m.input_channel = 0;
        if (d && !m.enabled) m.enabled = true;
        renderMicRows();
      },
    },
      h('option', { value: '' }, '— Choose a microphone —'),
      state.devices.map((d) => h('option', { value: d.id, selected: d.id === m.device_id, disabled: !!d.problem && d.id !== m.device_id }, d.name + (d.channels > 1 ? ` (${d.channels} inputs)` : '') + (d.is_default ? ' · default' : '') + (d.problem ? ` — can't be used: ${d.problem}` : ''))),
      m.device_id && !dev ? h('option', { value: m.device_id, selected: true }, `${m.device_name || 'Unknown microphone'} (not connected)`) : null
    );
    const inputSelect = dev && dev.channels > 1
      ? h('label', { class: 'dim' }, 'Input',
        h('select', { onchange: (e) => { m.input_channel = Number(e.target.value); } },
          Array.from({ length: dev.channels }, (_, c) => h('option', { value: String(c), selected: c === m.input_channel }, `Input ${c + 1}`))))
      : h('label', { class: 'dim' }, 'Input', h('select', { disabled: true }, h('option', {}, 'Input 1')));
    const gainOut = h('span', {}, `${m.gain_db > 0 ? '+' : ''}${m.gain_db} dB`);
    return h('div', { class: 'mic-row' + (m.enabled ? '' : ' off') },
      h('label', { class: 'switch' }, h('input', { type: 'checkbox', checked: m.enabled, onchange: (e) => { m.enabled = e.target.checked; renderMicRows(); } }), `Mic ${i + 1}`),
      h('label', { class: 'dim' }, 'Name (who uses it)', h('input', { type: 'text', maxlength: '40', value: m.name, placeholder: `Mic ${i + 1}`, oninput: (e) => { m.name = e.target.value; } })),
      h('label', { class: 'dim' }, 'Microphone', devSelect),
      inputSelect,
      h('label', { class: 'dim' }, 'Push-to-talk key',
        h('span', { class: 'key-field' },
          h('span', { class: 'keycap' }, keyLabel(m.key)),
          h('button', { type: 'button', class: 'btn', onclick: () => captureKey(i) }, 'Change'),
          m.key ? h('button', { type: 'button', class: 'btn btn-ghost', title: 'No key (use the on-screen button)', onclick: () => { m.key = ''; renderMicRows(); } }, '✕') : null)),
      h('div', { class: 'extra dim' },
        h('label', {}, 'Level in the mix',
          h('input', { type: 'range', min: '-12', max: '12', step: '1', value: String(m.gain_db), oninput: (e) => { m.gain_db = Number(e.target.value); gainOut.textContent = `${m.gain_db > 0 ? '+' : ''}${m.gain_db} dB`; } }),
          gainOut),
        h('label', {}, 'Sync delay',
          h('input', { type: 'number', min: '0', max: '500', step: '0.1', value: String(m.offset_ms), style: 'width:80px', oninput: (e) => { m.offset_ms = Math.max(0, Math.min(500, Number(e.target.value) || 0)); } }),
          'ms', h('span', { class: 'small' }, '(set by the clap test)')))
    );
  }

  function renderOptions() {
    $('folderPath').textContent = draft.save_folder || 'No folder chosen';
    $('prerollInput').value = draft.preroll_ms;
    $('tailInput').value = draft.tail_ms;
    $('keepWavInput').checked = !!draft.keep_wav;
    $('settingsPath').textContent = state.settings_path;
    $('version').textContent = state.version;
  }

  async function captureKey(slot) {
    capturingKey = true;
    try { await invoke('pause_keys', { paused: true }); } catch (_) { /* ignore */ }
    const overlay = $('keyCapture');
    const msg = overlay.querySelector('p');
    const original = msg.textContent;
    overlay.hidden = false;
    const finish = async (key) => {
      window.removeEventListener('keydown', onKey, true);
      overlay.hidden = true;
      msg.textContent = original;
      capturingKey = false;
      try { await invoke('pause_keys', { paused: false }); } catch (_) { /* ignore */ }
      if (key) {
        draft.mics[slot].key = key;
        renderMicRows();
      }
    };
    const onKey = async (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === 'Escape') return finish(null);
      if (['Control', 'Shift', 'Alt', 'Meta', 'OS'].includes(e.key)) return; // wait for the main key
      const mods = [];
      if (e.ctrlKey) mods.push('Control');
      if (e.altKey) mods.push('Alt');
      if (e.shiftKey) mods.push('Shift');
      if (e.metaKey) mods.push('Super');
      const key = [...mods, e.code || e.key].join('+');
      try {
        await invoke('check_key', { key });
        await finish(key);
      } catch (err) {
        msg.textContent = `${errText(err)} Try another key, or press Esc.`;
      }
    };
    window.addEventListener('keydown', onKey, true);
  }

  function validateDraft() {
    const on = draft.mics.map((m, i) => ({ m, i })).filter(({ m }) => m.enabled);
    for (const { m, i } of on) {
      if (!m.device_id) return `Mic ${i + 1} is switched on but has no microphone chosen.`;
    }
    for (let a = 0; a < on.length; a++) {
      for (let b = a + 1; b < on.length; b++) {
        const x = on[a];
        const y = on[b];
        if (x.m.device_id === y.m.device_id && x.m.input_channel === y.m.input_channel) {
          return `${micName(x.i, draft)} and ${micName(y.i, draft)} use the same microphone input.`;
        }
        if (x.m.key && x.m.key === y.m.key) return `${micName(x.i, draft)} and ${micName(y.i, draft)} have the same push-to-talk key.`;
      }
    }
    if (!draft.save_folder.trim()) return 'Choose a folder to save recordings in (Saving & options).';
    return '';
  }

  async function saveSetup() {
    draft.preroll_ms = Math.max(0, Math.min(2000, Number($('prerollInput').value) || 0));
    draft.tail_ms = Math.max(0, Math.min(2000, Number($('tailInput').value) || 0));
    draft.keep_wav = $('keepWavInput').checked;
    const problem = validateDraft();
    if (problem) {
      $('setupErr').textContent = problem;
      return;
    }
    $('saveSetup').disabled = true;
    $('setupErr').textContent = '';
    try {
      state = await invoke('save_settings', { settings: draft });
      $('setup').close();
      renderTiles();
      renderSaveTo();
      renderBanner();
    } catch (e) {
      $('setupErr').textContent = errText(e);
    } finally {
      $('saveSetup').disabled = false;
    }
  }

  function renderCheck() {
    const rows = [];
    if (!status || !status.running) {
      rows.push(h('tr', {}, h('td', { colspan: '6', class: 'small' }, 'No microphones are open. Switch mics on in the Microphones tab and press Save & apply.')));
    } else {
      status.mics.forEach((m, k) => {
        const slot = status.slots[k];
        const noise = m.noise_floor_db;
        const noiseTag = noise <= -60 ? ['good', 'Quiet'] : noise <= -48 ? ['mid', 'OK'] : ['bad', 'Noisy'];
        const drift = Math.abs(m.drift_ppm);
        const fill = h('div', { class: 'meter-fill' });
        fill.style.width = pct(m.rms_db) + '%';
        fill.style.setProperty('--full', pct(m.rms_db) > 0.5 ? `${(100 / pct(m.rms_db)) * 100}%` : '100%');
        rows.push(h('tr', {},
          h('td', {}, h('b', {}, micName(slot)), h('div', { class: 'small' }, m.device)),
          h('td', {}, h('div', { class: 'meter' }, fill), SIGNAL_MSG[m.signal] ? h('div', { class: 'small tile-err', title: SIGNAL_HELP[m.signal] }, SIGNAL_MSG[m.signal]) : null),
          h('td', {}, m.locked ? `${noise.toFixed(0)} dB` : '—', m.locked ? h('span', { class: 'tag ' + noiseTag[0] }, noiseTag[1]) : null),
          h('td', {}, m.locked ? `${m.drift_ppm >= 0 ? '+' : ''}${m.drift_ppm.toFixed(0)} ppm` : '—', m.locked ? h('span', { class: 'tag ' + (drift < 400 ? 'good' : 'bad') }, drift < 400 ? 'Corrected' : 'Unusually large') : null),
          h('td', {}, String(m.dropouts), m.dropouts ? h('span', { class: 'tag bad' }, 'Check cable / signal') : null),
          h('td', {}, `${(state.settings.mics[slot].offset_ms || 0).toFixed(1)} ms`)
        ));
      });
    }
    $('checkRows').replaceChildren(...rows);
  }

  async function runClapTest() {
    const msg = $('clapMsg');
    try {
      await invoke('clap_test_start');
    } catch (e) {
      msg.textContent = errText(e);
      return;
    }
    $('clapBtn').disabled = true;
    msg.textContent = 'Listening… clap once now!';
    const started = Date.now();
    const tick = async () => {
      try {
        const res = await invoke('clap_test_result');
        if (res) {
          state = res;
          for (const slot of state.active) {
            if (draft) draft.mics[slot].offset_ms = state.settings.mics[slot].offset_ms;
          }
          const parts = state.active.map((s) => `${micName(s)} ${state.settings.mics[s].offset_ms.toFixed(1)} ms`);
          msg.textContent = `Done — mics lined up. Delays: ${parts.join(', ')}.`;
          $('clapBtn').disabled = false;
          renderMicRows();
          renderTiles();
          return;
        }
      } catch (e) {
        msg.textContent = errText(e);
        $('clapBtn').disabled = false;
        return;
      }
      if (Date.now() - started > 15000) {
        msg.textContent = 'No result — try again.';
        $('clapBtn').disabled = false;
        return;
      }
      setTimeout(tick, 300);
    };
    setTimeout(tick, 600);
  }

  // ---------------------------------------------------------------- wiring
  $('recBtn').addEventListener('click', toggleRecording);
  $('setupBtn').addEventListener('click', openSetup);
  $('cancelSetup').addEventListener('click', () => $('setup').close());
  $('saveSetup').addEventListener('click', saveSetup);
  $('setup').addEventListener('cancel', (e) => { if (capturingKey) e.preventDefault(); });
  document.querySelectorAll('.tab').forEach((t) => t.addEventListener('click', () => showTab(t.dataset.tab)));
  $('clapBtn').addEventListener('click', runClapTest);
  $('openFolderBtn').addEventListener('click', () => { if (lastSaved) invoke('open_folder', { path: String(lastSaved.folder) }).catch((e) => { stopError = errText(e); }); });
  $('folderBtn').addEventListener('click', async () => {
    const p = await invoke('pick_folder');
    if (p) {
      draft.save_folder = p;
      renderOptions();
    }
  });
  $('refreshDevices').addEventListener('click', refreshDevices);
  $('caseInput').addEventListener('keydown', (e) => { if (e.key === 'Enter') toggleRecording(); });

  async function init() {
    state = await invoke('get_state');
    if (state.launch.case) $('caseInput').value = state.launch.case;
    renderTiles();
    renderSaveTo();
    renderBanner();
    setInterval(poll, 66);
    poll();
  }
  init().catch((e) => {
    $('banner').hidden = false;
    $('bannerText').textContent = `PathoSync could not start: ${errText(e)}`;
  });
})();
