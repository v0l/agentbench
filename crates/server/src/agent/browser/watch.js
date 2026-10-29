(() => {
  if (window.__twWatch) return;
  const w = { last: performance.now(), busy: new Map(), n: 0 };
  window.__twWatch = w;
  const start = () => {
    const id = ++w.n;
    w.busy.set(id, performance.now());
    return id;
  };
  const end = (id) => {
    w.busy.delete(id);
    w.last = performance.now();
  };
  const fetch = window.fetch;
  if (fetch) {
    window.fetch = function (...args) {
      const id = start();
      return fetch.apply(this, args).finally(() => end(id));
    };
  }
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function (...args) {
    const id = start();
    this.addEventListener('loadend', () => end(id));
    return send.apply(this, args);
  };
  new MutationObserver(() => {
    w.last = performance.now();
  }).observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
})();
