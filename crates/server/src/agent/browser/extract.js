(() => {
  const INTERACTIVE = 'a[href],button,input:not([type=hidden]),select,textarea,summary,[onclick],[role=button],[role=link],[role=checkbox],[role=switch],[role=tab],[role=menuitem],[role=option],[contenteditable=""],[contenteditable=true],[tabindex]:not([tabindex="-1"])';
  const SKIP = new Set(['script', 'style', 'noscript', 'template', 'svg', 'head', 'meta', 'link', 'iframe', 'canvas', 'option', 'optgroup']);
  const NATIVE = new Set(['a', 'button', 'input', 'select', 'textarea', 'summary']);
  const vw = innerWidth;
  const vh = innerHeight;
  const els = [];
  const nodes = [];
  const range = document.createRange();
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const clamp = (n, lo, hi) => Math.min(Math.max(n, lo), hi);
  const shown = (el) => {
    const s = getComputedStyle(el);
    return s.display !== 'none' && s.visibility !== 'hidden' && s.opacity !== '0';
  };
  const onScreen = (r) => r.bottom > 0 && r.top < vh && r.right > 0 && r.left < vw;
  const free = (el, r) => {
    const x = clamp(r.left + Math.min(r.width / 2, 12), 0, vw - 1);
    const y = clamp(r.top + Math.min(r.height / 2, 8), 0, vh - 1);
    const hit = document.elementFromPoint(x, y);
    return !hit || el.contains(hit) || hit.contains(el);
  };
  const add = (el, r, node, target) => {
    if (nodes.length >= 2500 || !r || (r.width === 0 && r.height === 0) || !onScreen(r) || !free(el, r)) return;
    node.x = r.left;
    node.y = r.top;
    node.w = r.width;
    node.h = r.height;
    if (node.interactive) {
      node.idx = els.length;
      els.push(target || el);
    }
    nodes.push(node);
  };
  const nameOf = (el) =>
    clean(el.innerText || el.textContent) ||
    clean(el.getAttribute('aria-label')) ||
    clean(el.title) ||
    clean(el.querySelector('img[alt]') && el.querySelector('img[alt]').alt);
  const interactiveNode = (el) => {
    const tag = el.localName;
    const type = (el.type || '').toLowerCase();
    const role = el.getAttribute('role');
    const n = { k: 'button', t: '', interactive: true, off: !!el.disabled || el.getAttribute('aria-disabled') === 'true' };
    if (tag === 'a') {
      n.k = 'link';
      n.t = nameOf(el);
    } else if (tag === 'input') {
      if (type === 'checkbox' || type === 'radio') {
        n.k = type;
        n.on = el.checked;
      } else if (['submit', 'button', 'reset', 'image'].includes(type)) {
        n.t = clean(el.value) || clean(el.alt) || clean(el.getAttribute('aria-label')) || type;
      } else if (type === 'file') {
        n.k = 'file';
        n.t = el.files && el.files[0] ? el.files[0].name : '';
      } else {
        n.k = 'input';
        n.t = type === 'password' ? '\u2022'.repeat(el.value.length) : el.value;
        n.hint = clean(el.placeholder) || clean(el.getAttribute('aria-label')) || clean(el.name);
      }
    } else if (tag === 'select') {
      n.k = 'select';
      n.t = el.selectedOptions[0] ? clean(el.selectedOptions[0].text) : '';
    } else if (tag === 'textarea') {
      n.k = 'textarea';
      n.t = el.value.replace(/\s*\n\s*/g, '\u23ce');
      n.hint = clean(el.placeholder) || clean(el.getAttribute('aria-label')) || clean(el.name);
    } else if (el.isContentEditable) {
      n.k = 'textarea';
      n.t = clean(el.innerText);
    } else if (role === 'checkbox' || role === 'switch') {
      n.k = 'checkbox';
      n.on = el.getAttribute('aria-checked') === 'true';
      n.t = nameOf(el);
    } else {
      n.t = nameOf(el);
    }
    n.t = n.t.slice(0, 120);
    return n;
  };
  const hiddenCheck = (el) => {
    const type = (el.type || '').toLowerCase();
    if (el.localName !== 'input' || (type !== 'checkbox' && type !== 'radio')) return;
    const label = el.labels && el.labels[0];
    if (!label || !shown(label)) return;
    add(label, label.getClientRects()[0], { k: type, t: '', interactive: true, on: el.checked, off: el.disabled });
  };
  const walk = (el) => {
    const tag = el.localName;
    if (SKIP.has(tag)) return;
    if (!shown(el)) {
      hiddenCheck(el);
      return;
    }
    if (el.matches(INTERACTIVE)) {
      const generic = !NATIVE.has(tag);
      if (!generic || (clean(el.innerText).length <= 80 && !el.querySelector(INTERACTIVE))) {
        add(el, el.getClientRects()[0], interactiveNode(el));
        return;
      }
    }
    if (/^h[1-6]$/.test(tag) && !el.querySelector(INTERACTIVE)) {
      const t = clean(el.innerText).slice(0, 160);
      if (t) add(el, el.getClientRects()[0], { k: 'heading', lvl: Number(tag[1]), t });
      return;
    }
    if (tag === 'img') {
      const alt = clean(el.alt);
      if (alt) add(el, el.getClientRects()[0], { k: 'img', t: alt.slice(0, 80) });
      return;
    }
    if (tag === 'hr') {
      add(el, el.getClientRects()[0], { k: 'rule', t: '' });
      return;
    }
    if (el.shadowRoot) for (const c of el.shadowRoot.children) walk(c);
    for (const c of el.childNodes) {
      if (c.nodeType === 3) {
        const t = clean(c.nodeValue);
        if (!t) continue;
        range.selectNodeContents(c);
        add(el, range.getClientRects()[0], { k: 'text', t });
      } else if (c.nodeType === 1) {
        walk(c);
      }
    }
  };
  if (document.body) walk(document.body);
  window.__twEls = els;
  return JSON.stringify({
    url: location.href,
    title: document.title,
    scrollY: Math.round(scrollY),
    scrollHeight: document.documentElement.scrollHeight,
    vw,
    vh,
    nodes,
  });
})()
