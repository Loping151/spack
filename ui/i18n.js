'use strict';

globalThis.SpackI18n = {
  create(registry, { storage = null, languages = ['en'], override = null } = {}) {
    const fallback = registry.find((entry) => entry.id === 'en');
    if (!fallback) throw new Error('English locale is required');
    const locales = new Map(registry.map((entry) => [entry.id, entry]));
    const nativeMessages = new Map();
    const own = (object, key) => Object.prototype.hasOwnProperty.call(object || {}, key);
    const normalize = (value) => String(value || '').replace(/_/g, '-').toLowerCase();
    const resolve = (value) => {
      const normalized = normalize(value);
      const exact = registry.find((entry) => normalize(entry.id) === normalized);
      if (exact) return exact;
      let match, specificity = 0;
      for (const entry of registry) {
        for (const alias of entry.matches) {
          const prefix = normalize(alias);
          if (prefix.length > specificity && (normalized === prefix || normalized.startsWith(prefix + '-'))) {
            match = entry; specificity = prefix.length;
          }
        }
      }
      return match;
    };
    let saved;
    try { saved = storage?.getItem('spack.language'); } catch (_) { /* Storage can be disabled by the host. */ }
    let active = resolve(override) || resolve(saved) || languages.map(resolve).find(Boolean) || fallback;
    const api = {
      get language() { return active.id; },
      get locales() { return registry.map(({ id, label }) => ({ id, label })); },
      setLanguage(id) {
        active = locales.get(id) || fallback;
        try { storage?.setItem('spack.language', active.id); } catch (_) { /* Language remains available for this session. */ }
      },
      setNative(id, messages) { nativeMessages.set(id, messages); },
      hasNative(id) { return nativeMessages.has(id); },
      has(key) {
        return own(active.messages, key) || own(fallback.messages, key)
          || own(nativeMessages.get(active.id), key) || own(nativeMessages.get('en'), key);
      },
      m(key, params = {}) { return { key, params }; },
      t(value, params = {}) {
        if (typeof value !== 'object' || value === null) return String(value ?? '');
        const key = value.key;
        if (typeof key !== 'string') return String(value);
        const catalogs = [active.messages, nativeMessages.get(active.id), fallback.messages, nativeMessages.get('en')];
        const catalog = catalogs.find((messages) => own(messages, key));
        const template = catalog ? catalog[key] : key;
        return template.replace(/\{([^{}]+)\}/g, (token, name) => {
          const args = value.params || params;
          if (!own(args, name)) return token;
          const argument = args[name];
          return typeof argument === 'number' ? api.number(argument) : api.t(argument);
        });
      },
      number(value, options = {}) { return new Intl.NumberFormat(active.id, options).format(value); },
      time(value) { return new Intl.DateTimeFormat(active.id, { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false }).format(value); },
      apply(root) {
        for (const element of root.querySelectorAll('[data-i18n]')) element.textContent = api.t(api.m(element.dataset.i18n));
        for (const attribute of ['title', 'aria-label']) {
          for (const element of root.querySelectorAll('[data-i18n-' + attribute + ']')) {
            element.setAttribute(attribute, api.t(api.m(element.getAttribute('data-i18n-' + attribute))));
          }
        }
      },
    };
    return api;
  },
};
