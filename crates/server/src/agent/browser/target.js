(i, mode) => {
  const el = window.__twEls instanceof Map ? window.__twEls.get(i) : undefined;
  if (!el || !el.isConnected) return { error: 'stale' };
  const a = el.closest('a');
  if (a && a.target === '_blank') a.target = '_self';
  el.scrollIntoView({ block: 'center', inline: 'center' });
  if (mode === 'focus') {
    el.focus();
    try {
      if (el.select) {
        el.select();
      } else {
        const range = document.createRange();
        range.selectNodeContents(el);
        const sel = getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
      }
    } catch (e) {}
    return { ok: true };
  }
  const r = el.getClientRects()[0] || el.getBoundingClientRect();
  const x = Math.min(Math.max(r.left + r.width / 2, 0), innerWidth - 1);
  const y = Math.min(Math.max(r.top + r.height / 2, 0), innerHeight - 1);
  const hit = document.elementFromPoint(x, y);
  const deco = hit && hit.parentElement === el.parentElement && !(hit.innerText || '').trim();
  if (hit && !el.contains(hit) && !hit.contains(el) && !deco) {
    const text = (hit.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 60);
    return { error: 'covered', by: hit.localName + (hit.id ? '#' + hit.id : '') + (text ? ' "' + text + '"' : '') };
  }
  return { x, y };
}
