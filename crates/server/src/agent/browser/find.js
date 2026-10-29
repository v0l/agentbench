(wanted, near) => {
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const lower = (s) => clean(s).toLowerCase();
  const els = window.__twEls instanceof Map ? [...window.__twEls] : [];
  const nameOf = (el) => {
    const own =
      clean(el.innerText) ||
      clean(el.getAttribute('aria-label')) ||
      clean(el.labels && el.labels[0] && el.labels[0].innerText) ||
      clean(el.value) ||
      clean(el.title) ||
      clean(el.placeholder);
    if (own) return own;
    const holder = el.parentElement;
    const around = holder && clean(holder.innerText);
    return around && around.length <= 80 ? around : '';
  };
  const target = lower(wanted);
  const named = els.filter(([, el]) => el.isConnected).map(([id, el]) => ({ id, el, name: lower(nameOf(el)) }));
  let hits = named.filter((c) => c.name === target);
  if (!hits.length) hits = named.filter((c) => c.name.includes(target));
  if (!hits.length) return { error: 'nothing on the page is labelled ' + JSON.stringify(wanted) + '; snapshot to see what is there' };
  const centre = (el) => {
    const r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  };
  if (near) {
    const anchor = lower(near);
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    const anchors = [];
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (lower(n.nodeValue).includes(anchor) && n.parentElement) anchors.push(n.parentElement);
    }
    for (const el of document.querySelectorAll('[aria-label]')) {
      if (lower(el.getAttribute('aria-label')).includes(anchor)) anchors.push(el);
    }
    if (!anchors.length) return { error: 'no text ' + JSON.stringify(near) + ' on the page to look near' };
    const cost = (c) => {
      let best = Infinity;
      const p = centre(c.el);
      for (const a of anchors) {
        const q = centre(a);
        const before = a.compareDocumentPosition(c.el) & Node.DOCUMENT_POSITION_FOLLOWING ? 0 : 2000;
        best = Math.min(best, Math.hypot(p.x - q.x, p.y - q.y) + before);
      }
      return best;
    };
    hits.sort((a, b) => cost(a) - cost(b));
    return { ref: hits[0].id };
  }
  if (hits.length > 1) {
    return {
      error:
        hits.length + ' elements are labelled ' + JSON.stringify(wanted) + ' (refs ' + hits.map((c) => c.id).join(', ') +
        '); add `near` with text close to the one you mean, or use its ref',
    };
  }
  return { ref: hits[0].id };
}
