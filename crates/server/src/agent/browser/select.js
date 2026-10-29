(i, wanted) => {
  const el = (window.__twEls || [])[i];
  if (!el || !el.isConnected) return { error: 'stale' };
  if (el.localName !== 'select') return { error: 'not a select' };
  const options = [...el.options];
  const lower = wanted.toLowerCase();
  const pick =
    options.find((o) => o.value === wanted) ||
    options.find((o) => o.text.trim() === wanted) ||
    options.find((o) => o.text.trim().toLowerCase() === lower) ||
    options.find((o) => o.text.trim().toLowerCase().includes(lower));
  if (!pick) return { error: 'no option ' + JSON.stringify(wanted) + ', options: ' + options.map((o) => JSON.stringify(o.text.trim())).join(', ') };
  el.value = pick.value;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: true };
}
