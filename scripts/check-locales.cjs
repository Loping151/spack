'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const read = (file) => fs.readFileSync(path.join(root, file), 'utf8');
const context = vm.createContext({ Intl });
const html = read('ui/index.html');
const scripts = [...html.matchAll(/<script src="([^"]+)"><\/script>/g)].map((match) => match[1]);
for (const script of scripts) {
  const source = read('ui/' + script);
  new vm.Script(source, { filename: script });
  if (script !== 'app.js') vm.runInContext(source, context, { filename: script });
}
const registry = context.SpackLocaleRegistry;
const english = registry.find(({ id }) => id === 'en').messages;
const keys = Object.keys(english).sort();
assert.equal(new Set(registry.map(({ id }) => id)).size, registry.length, 'Locale IDs must be unique');
const placeholders = (text) => [...text.matchAll(/\{([^{}]+)\}/g)].map((match) => match[1]).sort();
for (const locale of registry) {
  assert.deepEqual(Object.keys(locale.messages).sort(), keys, locale.id + ': translation keys');
  for (const key of keys) {
    assert.equal(typeof locale.messages[key], 'string', locale.id + ': ' + key);
    assert.ok(locale.messages[key].trim(), locale.id + ': empty ' + key);
    assert.deepEqual(placeholders(locale.messages[key]), placeholders(english[key]), locale.id + ': parameters in ' + key);
  }
}
const referenced = [
  ...[...html.matchAll(/data-i18n(?:-title|-aria-label)?="([^"]+)"/g)].map((match) => match[1]),
  ...[...read('ui/app.js').matchAll(/\b[mt]\('([^']+)'/g)].map((match) => match[1]).filter((key) => !key.endsWith('.')),
];
for (const key of referenced) assert.ok(Object.hasOwn(english, key), 'Missing UI message: ' + key);
const create = (options = {}, locales = registry) => context.SpackI18n.create(locales, options);
assert.equal(create({ languages: ['zh-Hans-CN'] }).language, 'zh-CN');
assert.equal(create({ languages: ['fr-FR'] }).language, 'en');
assert.equal(create({ languages: ['fr-FR', 'zh-CN'] }).language, 'zh-CN');
const extended = [...registry, { id: 'zh-TW', label: '繁體中文', matches: ['zh-TW', 'zh-Hant'], messages: english }];
for (const system of ['zh-Hant-HK', 'ZH_hAnT_hK', 'zh_tw', 'ZH-TW']) {
  assert.equal(create({ languages: [system] }, extended).language, 'zh-TW', 'Specific language alias: ' + system);
}
assert.equal(create({ languages: ['zh-Hans-CN'] }, extended).language, 'zh-CN');
assert.equal(create({ languages: ['en_US'] }, extended).language, 'en');
assert.equal(create({ override: 'ZH_hAnT_HK' }, extended).language, 'zh-TW');
let saved = 'en';
const storage = { getItem: () => saved, setItem: (_key, value) => { saved = value; } };
const i18n = create({ languages: ['zh-CN'], storage });
assert.equal(i18n.language, 'en', 'Saved preference precedes system language');
i18n.setLanguage('zh-CN');
assert.equal(saved, 'zh-CN');
assert.equal(create({ storage, override: 'en' }).language, 'en');
const brokenStorage = { getItem() { throw new Error('disabled'); }, setItem() { throw new Error('disabled'); } };
assert.doesNotThrow(() => create({ storage: brokenStorage }).setLanguage('zh-CN'));
const entry = i18n.m('summary.phaseDetail', { phase: i18n.m('phase.scan'), detail: 'C:\\Media\\{phase}.mov' });
assert.equal(i18n.t(entry), '扫描文件 · C:\\Media\\{phase}.mov');
i18n.setLanguage('en');
assert.equal(i18n.t(entry), 'Scanning files · C:\\Media\\{phase}.mov', 'Stored messages relocalize without changing path data');
const partial = create({}, [...registry, { id: 'fr', label: 'Français', matches: ['fr'], messages: { 'job.ready': 'Prêt' } }]);
partial.setLanguage('fr');
assert.equal(partial.t(partial.m('job.ready')), 'Prêt');
assert.equal(partial.t(partial.m('action.pack')), 'Compress', 'Missing translations fall back to English');
partial.setLanguage('unknown');
assert.equal(partial.language, 'en');
for (const locale of registry) {
  const file = 'src-tauri/locales/' + locale.id + '.json';
  if (fs.existsSync(path.join(root, file))) i18n.setNative(locale.id, JSON.parse(read(file)));
}
const nativeEntry = i18n.m('error.path_case_conflict', ['C:\\Media\\{1}', 'C:\\Media\\Folder']);
assert.ok(i18n.t(nativeEntry).includes('C:\\Media\\{1}'), 'Native placeholders do not rewrite paths');
i18n.setLanguage('zh-CN');
assert.ok(i18n.t(i18n.m('error.cancelled')).includes('取消'));
i18n.setLanguage('en');
assert.ok(i18n.t(i18n.m('error.cancelled')).includes('cancelled'));
const listeners = [];
let releaseCatalog;
const startupContext = vm.createContext({
  native: true, preview: false, unlistenDrop: null,
  i18n: { locales: [] }, document: { createElement: () => ({}) },
  $: () => ({ replaceChildren() {} }), m: (key) => key,
  setText() {}, detail() {}, updateSplit() {}, renderLanguage() {},
  handleProgress() {}, handleNativeDrop() {}, log() {}, message: String,
  loadNativeLocales: () => new Promise((resolve) => { releaseCatalog = resolve; }),
  window: { __TAURI__: {
    event: { listen: () => { listeners.push('progress'); return Promise.resolve(() => {}); } },
    webview: { getCurrentWebview: () => ({ onDragDropEvent: () => { listeners.push('drop'); return Promise.resolve(() => {}); } }) },
  } },
});
const appSource = read('ui/app.js');
const startup = vm.runInContext(appSource.slice(appSource.indexOf('async function init()')), startupContext);
assert.deepEqual(listeners, ['progress', 'drop'], 'Native listeners must start before the translation catalog resolves');
releaseCatalog();
const videoLiteral = appSource.match(/const videoExtensions = (\[[\s\S]*?\]);/)[1];
const videoExtensions = [...vm.runInNewContext(videoLiteral)];
const backendVideoList = read('src-tauri/src/core/scan.rs').match(/const VIDEO_EXTENSIONS:\s*&\[&str\]\s*=\s*&\[([\s\S]*?)\];/)[1];
const backendVideoExtensions = [...backendVideoList.matchAll(/"([a-z0-9]+)"/g)].map((match) => match[1]);
assert.deepEqual([...videoExtensions].sort(), backendVideoExtensions.sort(), 'Frontend and backend video formats must match');
assert.equal(new Set(videoExtensions).size, videoExtensions.length, 'Video formats must not repeat');
const formatsContext = vm.createContext({ videoExtensions });
vm.runInContext(appSource.match(/const mediaExtensions = [^;]+;/)[0], formatsContext);
for (const name of ['archivePath', 'mediaPath']) {
  vm.runInContext(appSource.match(new RegExp('function ' + name + '\\(path\\) \\{[^\\n]+\\}'))[0], formatsContext);
}
for (const extension of [...videoExtensions, 'gif', 'png', 'webp']) {
  assert.ok(formatsContext.mediaPath('C:\\Media\\sample.' + extension.toUpperCase()), 'Media format: ' + extension);
}
assert.equal(formatsContext.mediaPath('C:\\Media\\sample.zip'), false);
for (const name of ['Exports.spk', 'Exports.SPK.001', 'Exports.spack', 'Exports.SPACK.004']) {
  assert.ok(formatsContext.archivePath(name), 'Archive compatibility: ' + name);
}
for (const name of ['Exports.spk.bak', 'Exports.spack.txt', 'Exports.zip']) assert.equal(formatsContext.archivePath(name), false);
const filterOptions = [...html.match(/<select id="fileFilter">([\s\S]*?)<\/select>/)[1].matchAll(/value="([^"]+)"/g)].map((match) => match[1]);
assert.deepEqual(filterOptions, ['all', 'gif', 'video', 'no-gif', 'no-video']);
const previewContext = vm.createContext({ previewDelay: () => Promise.resolve() });
vm.runInContext(appSource.slice(appSource.indexOf('async function previewInvoke('), appSource.indexOf('async function init()')), previewContext);
const previewChecks = filterOptions.map(async (filter) => {
  const stats = await previewContext.previewInvoke('scan_selection', { filter });
  assert.equal(stats.files, stats.gif + stats.video + stats.png + stats.webp, 'Video aggregate must not double-count MOV');
  assert.ok(stats.mov <= stats.video);
  if (filter === 'video') assert.equal(stats.files, stats.video);
  if (filter === 'gif') assert.equal(stats.files, stats.gif);
  if (filter === 'no-video') assert.equal(stats.video, 0);
  if (filter === 'no-gif') assert.equal(stats.gif, 0);
});
Promise.all([startup, ...previewChecks]).then(() => {
  console.log('Locale checks passed: ' + registry.length + ' languages, ' + keys.length + ' UI keys; language fallback, native messages, video formats and archive compatibility.');
}).catch((error) => { console.error(error); process.exitCode = 1; });
