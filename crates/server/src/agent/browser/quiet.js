(() => {
  const w = window.__twWatch;
  const now = performance.now();
  const body = document.body;
  const blank =
    location.href !== 'about:blank' &&
    (!body || (!body.innerText.trim() && !body.querySelector('img,input,button,select,textarea,canvas,svg,video,iframe')));
  return {
    ready: document.readyState,
    watched: !!w,
    idle: w ? now - w.last : 0,
    busy: w ? [...w.busy.values()].filter((t) => now - t < 5000).length : 0,
    blank,
  };
})()
