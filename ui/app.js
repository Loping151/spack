'use strict';

const $ = (id) => document.getElementById(id);
const native = Boolean(window.__TAURI__?.core?.invoke);
const preview = !native && new URLSearchParams(location.search).get('preview') === '1';
let localeStorage;
try { localeStorage = window.localStorage; } catch (_) { localeStorage = null; }
const i18n = SpackI18n.create(SpackLocaleRegistry, {
  storage: localeStorage, languages: navigator.languages || [navigator.language],
  override: preview ? new URLSearchParams(location.search).get('lang') : null,
});
const m = (key, params) => i18n.m(key, params);
const t = (key, params) => i18n.t(m(key, params));
const textBindings = new Map();
const videoExtensions = ['mov', 'mp4', 'm4v', 'mkv', 'webm', 'avi', 'wmv', 'flv', 'mpg', 'mpeg', 'm2v', 'ts', 'mts', 'm2ts', '3gp', '3g2', 'ogv', 'vob', 'mxf'];
const mediaExtensions = ['gif', ...videoExtensions, 'png', 'webp'];
const state = {
  tab: 'pack', dirs: [], scan: null, archive: null, archivePath: null,
  outDir: null, targetDir: null, busy: false, operation: null,
  logs: [], resultPath: null, lastPhase: null, scanToken: 0, cancelRequested: false,
};
const phases = new Set(['scan', 'transform', 'compress', 'split', 'verify', 'unpack']);
const lockIds = ['tabPack', 'tabUnpack', 'addFiles', 'addFolders', 'pickArchive', 'fileFilter', 'splitMode', 'splitValue', 'pickOutput', 'resetOutput', 'pickTarget', 'resetTarget'];
let pendingProgress = null;
let progressFrame = 0;
let unlistenDrop = null;

function fmtBytes(value) {
  if (value == null || !Number.isFinite(Number(value))) return '—';
  let n = Number(value), i = 0;
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i += 1; }
  return `${i ? n.toFixed(n >= 100 ? 1 : 2) : n.toFixed(0)} ${units[i]}`;
}
function basename(path) { return String(path).replace(/[\\/]+$/, '').split(/[\\/]/).pop(); }
function pathKey(path) {
  if (/^(?:[a-z]:[\\/]|\\\\|\/\/)/i.test(path)) return path.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();
  return path.replace(/\/+$/, '');
}
function archivePath(path) { return /\.(?:spk|spack)(?:\.\d+)?$/i.test(path); }
function mediaPath(path) { return mediaExtensions.includes(String(path).split('.').pop().toLowerCase()); }
function message(error) {
  if (error?.key) return error;
  const value = typeof error === 'string' ? error : error?.message || String(error);
  if (/^(?:error|codec)\.[\w.]+$/.test(value)) return m(value);
  try {
    const parsed = JSON.parse(value);
    if (/^(?:error|codec)\.[\w.]+$/.test(parsed?.code) && Array.isArray(parsed.args)) return m(parsed.code, parsed.args);
  } catch (_) { /* System diagnostics may be plain text. */ }
  return value;
}
function icon(name, className = '') {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttribute('href', `#i-${name}`); svg.append(use);
  if (className) svg.setAttribute('class', className);
  svg.setAttribute('aria-hidden', 'true');
  return svg;
}
function setText(id, value) { textBindings.set(id, value); $(id).textContent = i18n.t(value); $(id).title = i18n.t(value); }
function detail(text) { setText('jobDetail', text); }
function renderLogs() {
  if (!state.logs.length) {
    const li = document.createElement('li'); li.className = 'log-empty'; li.textContent = t('job.noLog');
    $('logList').replaceChildren(li); return;
  }
  $('logList').replaceChildren(...state.logs.slice(-2).map((entry) => {
    const time = i18n.time(entry.time), text = i18n.t(entry.text);
    const li = document.createElement('li'); li.className = entry.kind; li.title = `[${time}] ${text}`;
    const stamp = document.createElement('time'); stamp.textContent = time;
    li.append(stamp, document.createTextNode(text)); return li;
  }));
}
function log(text, kind = '') {
  state.logs.push({ time: new Date(), text, kind });
  if (state.logs.length > 1000) state.logs.shift();
  renderLogs();
  $('copyLog').disabled = false;
}
function reportError(error) {
  const text = message(error), cancelled = ['error.cancelled', 'job.cancelled'].includes(text?.key) || /^(?:operation )?cancelled\.?$/i.test(i18n.t(text));
  state.resultPath = null; $('copyResult').hidden = true;
  setText('jobTitle', m(cancelled ? 'job.cancelled' : 'job.failed'));
  $('jobCard').dataset.state = cancelled ? 'idle' : 'error';
  $('progressTrack').classList.remove('indeterminate');
  detail(text); log(text, cancelled ? '' : 'error'); setText('statusLine', m(cancelled ? 'job.cancelled' : 'job.failed'));
}
function setBusy(busy, operation = null) {
  state.busy = busy; state.operation = operation;
  if (busy) setDropHover(false);
  lockIds.forEach((id) => { $(id).disabled = busy; });
  document.querySelectorAll('[name="preset"], .remove-folder').forEach((node) => { node.disabled = busy; });
  $('cancelJob').hidden = !busy || !['pack', 'unpack'].includes(operation);
  $('cancelJob').disabled = false; setText('cancelJob', m('action.cancel'));
  $('startJob').hidden = ['pack', 'unpack'].includes(operation);
  updateStart();
}
function updateStart() {
  $('startJob').disabled = state.busy || (!native && !preview) || (state.tab === 'pack' ? !state.scan?.files : !state.archive);
  setText('startLabel', m(state.tab === 'pack' ? 'action.startPack' : 'action.startUnpack'));
}
function resetJob(title) {
  state.lastPhase = null; state.resultPath = null; state.cancelRequested = false;
  $('copyResult').hidden = true; $('jobCard').dataset.state = 'busy';
  setText('jobTitle', title); setText('progressPercent', ''); detail(m('job.preparing'));
  $('progressFill').style.width = '0%'; $('progressTrack').classList.add('indeterminate');
  $('progressTrack').removeAttribute('aria-valuenow'); setText('statusLine', title);
}
function finishJob(title, summary, path = null) {
  flushProgress();
  $('jobCard').dataset.state = 'success'; $('progressTrack').classList.remove('indeterminate');
  $('progressFill').style.width = '100%'; $('progressTrack').setAttribute('aria-valuenow', '100');
  setText('jobTitle', title); setText('progressPercent', '100%'); detail(summary);
  setText('statusLine', title); log(summary); state.resultPath = path; $('copyResult').hidden = !path;
  if (path) log(path);
}
async function invoke(command, args = {}) {
  if (native) return window.__TAURI__.core.invoke(command, args);
  if (preview) return previewInvoke(command, args);
  throw m('ui.error.nativeRequired');
}
async function openDialog(options) {
  if (preview) {
    if (!options.directory) return options.multiple ? ['C:\\Media\\sample.gif', 'C:\\Media\\sample.mp4'] : 'C:\\Media\\Exports.spk.001';
    return options.multiple ? ['C:\\Media\\GIF', 'C:\\Media\\Videos'] : 'C:\\Media';
  }
  if (!native) throw m('ui.error.nativeRequired');
  const dialog = window.__TAURI__.dialog || window.__TAURI_PLUGIN_DIALOG__;
  if (typeof dialog?.open === 'function') return dialog.open(options);
  return window.__TAURI__.core.invoke('plugin:dialog|open', { options });
}
async function guard(action) { try { await action(); } catch (error) { reportError(error); } }

function renderFolders() {
  setText('folderCount', String(state.dirs.length));
  const list = $('folderList'); list.replaceChildren();
  if (!state.dirs.length) {
    const empty = document.createElement('div'); empty.className = 'empty-state';
    const title = document.createElement('strong'); title.textContent = t('source.emptyTitle');
    const note = document.createElement('span'); note.textContent = t('source.emptyNote');
    empty.append(icon('folder', 'empty-icon'), title, note); list.append(empty); return;
  }
  state.dirs.forEach((path, index) => {
    const row = document.createElement('div'); row.className = 'folder-row';
    const content = document.createElement('div'); content.className = 'folder-content';
    const name = document.createElement('div'); name.className = 'folder-name'; name.textContent = basename(path); name.title = path;
    const sub = document.createElement('div'); sub.className = 'folder-path'; sub.textContent = path; sub.title = path;
    content.append(name, sub);
    const remove = document.createElement('button'); remove.className = 'icon-button remove-folder'; remove.disabled = state.busy;
    remove.title = t('action.remove', { name: basename(path) }); remove.setAttribute('aria-label', remove.title); remove.append(icon('x'));
    remove.addEventListener('click', () => guard(async () => { if (state.busy) return; state.dirs.splice(index, 1); renderFolders(); await scanSelection(); }));
    row.append(icon(mediaPath(path) ? 'file' : 'folder'), content, remove); list.append(row);
  });
}
function renderStats() {
  const scan = state.scan;
  setText('sourceBytes', scan ? fmtBytes(scan.bytes) : '—');
  setText('sourceFiles', scan ? i18n.number(scan.files) : '—');
  $('typeCounts').replaceChildren(...['gif', 'video', 'png', 'webp'].map((type) => {
    const span = document.createElement('span'), count = document.createElement('b');
    const label = type === 'video' ? t('source.video') : type.toUpperCase();
    count.textContent = scan ? String(scan[type] || 0) : '—'; span.append(document.createTextNode(`${label} `), count); return span;
  }));
  updateStart();
}
async function scanSelection() {
  const token = ++state.scanToken; state.scan = null; renderStats();
  if (!state.dirs.length) { detail(m('job.selectSources')); setText('statusLine', m('job.ready')); return; }
  setBusy(true, 'scan'); resetJob(m('phase.scan'));
  try {
    const scan = await invoke('scan_selection', { dirs: [...state.dirs], filter: $('fileFilter').value });
    if (token !== state.scanToken) return;
    state.scan = scan; renderStats(); flushProgress();
    const summary = m('summary.files', { files: scan.files, bytes: fmtBytes(scan.bytes) });
    finishJob(m(scan.files ? 'job.scanComplete' : 'job.noFiles'), summary);
    if (!scan.files) detail(m('job.noMatches'));
  } catch (error) { reportError(error); }
  finally { if (token === state.scanToken) setBusy(false); }
}
function updateOutputPaths() {
  setText('outputPath', state.outDir || m('settings.sourceParent')); $('resetOutput').hidden = !state.outDir;
  setText('targetPath', state.targetDir || m('settings.archiveParent')); $('resetTarget').hidden = !state.targetDir;
  setText('targetName', state.archive?.dir_name || '—');
}
function updateSplit(resetValue = true) {
  const mode = $('splitMode').value;
  $('splitValueWrap').hidden = mode === 'none';
  $('splitValue').min = mode === 'count' ? '2' : '1';
  $('splitValue').max = mode === 'count' ? '10000' : '1048576';
  if (resetValue) $('splitValue').value = mode === 'count' ? '2' : '100';
  $('splitValue').setAttribute('aria-label', t(mode === 'count' ? 'settings.partCount' : 'settings.partSize'));
  setText('splitUnit', mode === 'count' ? m('settings.parts') : 'MiB');
}
function switchTab(tab) {
  if (state.busy) return;
  setDropHover(false);
  state.tab = tab;
  for (const side of ['pack', 'unpack']) {
    const active = side === tab, suffix = side === 'pack' ? 'Pack' : 'Unpack';
    $(`tab${suffix}`).classList.toggle('active', active); $(`tab${suffix}`).setAttribute('aria-selected', String(active));
    $(`tab${suffix}`).tabIndex = active ? 0 : -1;
    $(`panel${suffix}`).classList.toggle('active', active); $(`panel${suffix}`).hidden = !active;
  }
  if (!state.logs.length) detail(m(tab === 'pack' ? 'job.selectSources' : 'job.selectArchive'));
  updateStart();
}
async function selectArchive() {
  if (state.busy) return;
  const selected = await openDialog({ title: t('dialog.archive'), multiple: false, directory: false, filters: [{ name: t('dialog.archiveFilter'), extensions: ['*'] }] });
  const path = Array.isArray(selected) ? selected[0] : selected;
  if (path) await loadArchive(path);
}
async function loadArchive(path) {
  if (state.busy || !path) return;
  state.archive = null; state.archivePath = path; updateStart(); updateOutputPaths();
  setText('archiveBytes', '—'); setText('archiveFiles', '—'); setText('archiveParts', '—');
  const box = $('archiveSelection'); box.replaceChildren(); box.classList.add('selected');
  const file = document.createElement('div'); file.className = 'archive-file';
  const name = document.createElement('strong'); name.textContent = basename(path); name.title = path;
  const location = document.createElement('span'); location.textContent = path; location.title = path;
  file.append(name, location); box.append(icon('box', 'empty-icon'), file);
  setBusy(true, 'info'); resetJob(m('job.readArchive'));
  try {
    state.archive = await invoke('archive_info', { packPath: path });
    setText('archiveBytes', fmtBytes(state.archive.bytes)); setText('archiveFiles', i18n.number(state.archive.files));
    const parts = Array.isArray(state.archive.parts) ? state.archive.parts.length : Number(state.archive.parts || 1);
    setText('archiveParts', String(parts)); updateOutputPaths();
    finishJob(m('job.archiveReady'), m('summary.archive', { files: state.archive.files, bytes: fmtBytes(state.archive.bytes), parts }));
  } catch (error) { reportError(error); }
  finally { setBusy(false); }
}

async function addSources(paths) {
  if (state.busy) return;
  const existing = new Set(state.dirs.map(pathKey));
  const previous = state.dirs.length;
  for (const path of paths) {
    if (typeof path !== 'string' || !path || path.includes('\0')) continue;
    const key = pathKey(path);
    if (!existing.has(key)) { existing.add(key); state.dirs.push(path); }
  }
  if (previous !== state.dirs.length) { renderFolders(); await scanSelection(); }
}
function setDropHover(active) {
  for (const tab of ['pack', 'unpack']) {
    $(`${tab}DropTarget`).classList.toggle('drop-active', Boolean(active && !state.busy && state.tab === tab));
  }
}
async function handleDroppedPaths(paths) {
  setDropHover(false);
  if (state.busy || !Array.isArray(paths)) return;
  const selected = paths.filter((path) => typeof path === 'string' && path.length && !path.includes('\0'));
  if (!selected.length) return;
  const archives = selected.filter(archivePath);
  if (state.tab === 'unpack' || archives.length) {
    if (!archives.length) throw m('ui.error.selectArchive');
    if (archives.length !== selected.length) throw m('ui.error.archiveSeparate');
    const packages = new Set(archives.map((path) => pathKey(path.replace(/(\.(?:spk|spack))\.\d+$/i, '$1'))));
    if (packages.size !== 1) throw m('ui.error.oneArchive');
    switchTab('unpack');
    await loadArchive(archives[0]);
  } else {
    await addSources(selected);
  }
}
function handleNativeDrop(event) {
  const payload = event?.payload;
  if (payload?.type === 'drop') return guard(() => handleDroppedPaths(payload.paths));
  setDropHover(payload?.type === 'enter' || payload?.type === 'over');
}

function handleProgress(progress) {
  if (!state.busy || !progress?.phase) return;
  pendingProgress = progress;
  if (!progressFrame) progressFrame = requestAnimationFrame(flushProgress);
}
function flushProgress() {
  if (progressFrame) cancelAnimationFrame(progressFrame);
  progressFrame = 0;
  const p = pendingProgress; pendingProgress = null;
  if (!p) return;
  const label = phases.has(p.phase) ? m('phase.' + p.phase) : p.phase, total = Number(p.total), done = Number(p.done);
  const known = Number.isFinite(total) && total > 0;
  const percent = known ? Math.max(0, Math.min(100, done / total * 100)) : 0;
  setText('jobTitle', label); setText('progressPercent', known ? `${percent.toFixed(0)}%` : '');
  $('progressTrack').classList.toggle('indeterminate', !known); $('progressFill').style.width = known ? `${percent}%` : '';
  if (known) $('progressTrack').setAttribute('aria-valuenow', String(Math.round(percent)));
  else $('progressTrack').removeAttribute('aria-valuenow');
  if (p.detail) detail(p.detail);
  if (p.phase !== state.lastPhase) { log(p.detail ? m('summary.phaseDetail', { phase: label, detail: p.detail }) : label); state.lastPhase = p.phase; }
  setText('statusLine', state.cancelRequested ? m('job.cancelling') : label);
}
async function runJob() {
  if (state.busy || $('startJob').disabled) return;
  const side = state.tab;
  let splitValue = 0;
  if (side === 'pack' && $('splitMode').value !== 'none') {
    splitValue = Number($('splitValue').value);
    const min = $('splitMode').value === 'count' ? 2 : 1, max = $('splitMode').value === 'count' ? 10000 : 1048576;
    if (!Number.isSafeInteger(splitValue) || splitValue < min || splitValue > max) {
      reportError(m('ui.error.splitRange', { min, max })); $('splitValue').focus(); return;
    }
  }
  setBusy(true, side); resetJob(m(side === 'pack' ? 'action.startPack' : 'action.startUnpack'));
  log(side === 'pack' ? m('summary.startPack', { files: state.scan.files }) : m('summary.startUnpack', { name: basename(state.archivePath) }));
  try {
    if (side === 'pack') {
      const result = await invoke('pack', { dirs: [...state.dirs], filter: $('fileFilter').value, preset: document.querySelector('[name="preset"]:checked').value, outDir: state.outDir, splitMode: $('splitMode').value, splitValue });
      const ratio = result.src_bytes > 0 ? `${(result.packed_bytes / result.src_bytes * 100).toFixed(1)}%` : '—';
      const parts = result.parts?.length || 1;
      finishJob(m('job.packComplete'), m('summary.pack', { source: fmtBytes(result.src_bytes), packed: fmtBytes(result.packed_bytes), ratio, parts }), result.pack_path);
    } else {
      const result = await invoke('unpack', { packPath: state.archivePath, targetDir: state.targetDir });
      finishJob(m('job.unpackComplete'), m('summary.unpack', { files: result.n_files, bytes: fmtBytes(result.bytes_written) }), result.dir);
    }
  } catch (error) { flushProgress(); reportError(error); }
  finally { pendingProgress = null; setBusy(false); }
}
async function copy(text) {
  try { await navigator.clipboard.writeText(text); }
  catch (_) {
    const input = document.createElement('textarea'); input.value = text; input.style.position = 'fixed'; input.style.opacity = '0';
    document.body.append(input); input.select(); const success = document.execCommand('copy'); input.remove();
    if (!success) throw m('ui.error.clipboard');
  }
  setText('statusLine', m('job.copied'));
}

function renderLanguage() {
  document.documentElement.lang = i18n.language;
  $('language').value = i18n.language;
  i18n.apply(document);
  for (const [id, value] of textBindings) setText(id, value);
  const scroll = $('folderList').scrollTop;
  renderFolders(); $('folderList').scrollTop = scroll;
  renderStats(); updateOutputPaths(); updateSplit(false); renderLogs();
  if (state.archive) setText('archiveFiles', i18n.number(state.archive.files));
}
const localeRequests = new Map();
async function loadNativeLocale(language) {
  if (!native || i18n.hasNative(language)) return;
  if (!localeRequests.has(language)) {
    const request = window.__TAURI__.core.invoke('locale_messages', { lang: language })
      .then((messages) => i18n.setNative(language, messages))
      .finally(() => localeRequests.delete(language));
    localeRequests.set(language, request);
  }
  await localeRequests.get(language);
}
async function loadNativeLocales() {
  const outcomes = await Promise.allSettled([...new Set(['en', i18n.language])].map(loadNativeLocale));
  for (const outcome of outcomes) {
    if (outcome.status === 'rejected') log(m('ui.error.locale', { detail: message(outcome.reason) }), 'error');
  }
  renderLanguage();
}
$('language').addEventListener('change', () => guard(async () => {
  i18n.setLanguage($('language').value); renderLanguage(); await loadNativeLocales();
}));
$('tabPack').addEventListener('click', () => switchTab('pack'));
$('tabUnpack').addEventListener('click', () => switchTab('unpack'));
document.querySelector('.tabs').addEventListener('keydown', (event) => {
  if (!['ArrowLeft', 'ArrowRight'].includes(event.key) || state.busy) return;
  event.preventDefault(); switchTab(state.tab === 'pack' ? 'unpack' : 'pack'); $(`tab${state.tab === 'pack' ? 'Pack' : 'Unpack'}`).focus();
});
$('addFolders').addEventListener('click', () => guard(async () => {
  if (state.busy) return;
  const selected = await openDialog({ title: t('dialog.folders'), directory: true, multiple: true });
  if (!selected) return;
  await addSources(Array.isArray(selected) ? selected : [selected]);
}));
$('addFiles').addEventListener('click', () => guard(async () => {
  if (state.busy) return;
  const selected = await openDialog({ title: t('dialog.files'), directory: false, multiple: true, filters: [{ name: t('dialog.mediaFilter'), extensions: mediaExtensions }] });
  if (selected) await addSources(Array.isArray(selected) ? selected : [selected]);
}));
$('fileFilter').addEventListener('change', () => guard(scanSelection));
$('splitMode').addEventListener('change', updateSplit);
$('pickOutput').addEventListener('click', () => guard(async () => { const path = await openDialog({ title: t('dialog.output'), directory: true, multiple: false }); if (path) state.outDir = Array.isArray(path) ? path[0] : path; updateOutputPaths(); }));
$('resetOutput').addEventListener('click', () => { state.outDir = null; updateOutputPaths(); });
$('pickTarget').addEventListener('click', () => guard(async () => { const path = await openDialog({ title: t('dialog.target'), directory: true, multiple: false }); if (path) state.targetDir = Array.isArray(path) ? path[0] : path; updateOutputPaths(); }));
$('resetTarget').addEventListener('click', () => { state.targetDir = null; updateOutputPaths(); });
$('pickArchive').addEventListener('click', () => guard(selectArchive));
$('startJob').addEventListener('click', () => guard(runJob));
$('cancelJob').addEventListener('click', () => guard(async () => {
  state.cancelRequested = true; $('cancelJob').disabled = true; setText('cancelJob', m('job.cancelling')); setText('statusLine', m('job.cancelling')); log(m('job.cancelRequested'));
  try { await invoke('cancel'); }
  catch (error) { state.cancelRequested = false; $('cancelJob').disabled = false; setText('cancelJob', m('action.cancel')); log(message(error), 'error'); }
}));
$('copyResult').addEventListener('click', () => guard(() => copy(state.resultPath)));
$('copyLog').addEventListener('click', () => guard(() => copy(state.logs.map((entry) => `[${i18n.time(entry.time)}] ${i18n.t(entry.text)}`).join('\n'))));
window.addEventListener('dragover', (event) => { event.preventDefault(); });
window.addEventListener('drop', (event) => { event.preventDefault(); });
window.addEventListener('blur', () => setDropHover(false));
window.addEventListener('beforeunload', () => { unlistenDrop?.(); });

const previewDelay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function previewInvoke(command, args) {
  if (command === 'cancel') return;
  if (command === 'scan_selection') {
    await previewDelay(180);
    const counts = { gif: 96, video: 96, png: 4, webp: 2 };
    if (args.filter === 'gif' || args.filter === 'video') for (const key of Object.keys(counts)) if (key !== args.filter) counts[key] = 0;
    if (args.filter === 'no-gif') counts.gif = 0;
    if (args.filter === 'no-video') counts.video = 0;
    return { ...counts, mov: counts.video ? 80 : 0, files: Object.values(counts).reduce((a, b) => a + b, 0), bytes: counts.video * 84567200 + counts.gif * 845678 + counts.png * 251000 + counts.webp * 132000 };
  }
  if (command === 'archive_info') return { files: 198, bytes: 8200000000, dir_name: 'Exports', parts: 4 };
  for (const phase of command === 'pack' ? ['scan', 'transform', 'compress', 'split', 'verify'] : ['unpack', 'verify']) {
    for (let done = 0; done <= 10; done += 1) {
      if (state.cancelRequested) throw m('job.cancelled');
      handleProgress({ phase, done, total: 10, detail: `Videos / animation_${String(done + 1).padStart(3, '0')}.mp4` }); await previewDelay(55);
    }
  }
  if (command === 'pack') return { pack_path: 'C:\\Media\\Exports.spk', parts: ['Exports.spk'], src_bytes: state.scan.bytes, packed_bytes: Math.round(state.scan.bytes * .23), n_files: state.scan.files };
  return { dir: 'C:\\Media\\Exports', n_files: 198, bytes_written: 8200000000 };
}
async function init() {
  $('language').replaceChildren(...i18n.locales.map(({ id, label }) => {
    const option = document.createElement('option'); option.value = id; option.textContent = label; return option;
  }));
  setText('headerNote', m('app.lossless')); setText('jobTitle', m('job.ready'));
  setText('statusLine', m('job.ready')); setText('cancelJob', m('action.cancel'));
  detail(m('job.selectSources')); updateSplit(); renderLanguage();
  if (preview) { setText('headerNote', m('app.preview')); setText('statusLine', m('job.previewStatus')); }
  else if (!native) { setText('headerNote', m('app.staticPreview')); detail(m('job.nativeRequired')); }
  if (native) {
    const outcomes = await Promise.allSettled([
      (async () => window.__TAURI__.event.listen('spack://progress', (event) => handleProgress(event.payload)))(),
      (async () => { unlistenDrop = await window.__TAURI__.webview.getCurrentWebview().onDragDropEvent(handleNativeDrop); })(),
      loadNativeLocales(),
    ]);
    for (const [index, outcome] of outcomes.entries()) {
      if (outcome.status === 'rejected') {
        log(m(['ui.error.progress', 'ui.error.drop', 'ui.error.locale'][index], { detail: message(outcome.reason) }), 'error');
      }
    }
  }
}
init();
