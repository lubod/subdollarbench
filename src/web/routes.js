// Round 39: the console's pure route table and header Live button.
// Kept in its own served file so node can test them without a DOM.
(function (root) {
  'use strict';

  function clean(hash) {
    return decodeURIComponent(String(hash || '').replace(/^#/, ''));
  }

  function routeFor(hash, cells, previousView) {
    const h = clean(hash);
    const list = Array.isArray(cells) ? cells : [];
    if (h === 'run' || h.indexOf('run/') === 0) {
      const id = h.indexOf('run/') === 0 ? h.slice(4) : '';
      return { view: id ? 'run' : 'board', runId: id || null, overlay: null };
    }
    if (h === 'admin') return { view: 'admin', runId: null, overlay: null };
    if (h === 'live' || h.indexOf('live/') === 0) {
      let id = h.indexOf('live/') === 0 ? h.slice(5) : '';
      if (!id) {
        const live = list.find(c => c && (c.state === 'running' || c.state === 'stalled'));
        id = live ? live.id : '';
      }
      if (!id) return { view: 'board', runId: null, overlay: null };
      return {
        view: previousView === 'run' || previousView === 'admin' ? previousView : 'board',
        runId: null,
        overlay: { id: id },
      };
    }
    return { view: 'board', runId: null, overlay: null };
  }

  function liveButton(cells) {
    const list = Array.isArray(cells) ? cells : [];
    const cell = list.find(c => c && (c.state === 'running' || c.state === 'stalled'));
    if (!cell) return null;
    if (cell.state === 'stalled') {
      return { id: cell.id, text: 'Live · stalled', stalled: true };
    }
    const live = cell.live || {};
    const spent = live.campaign_spent != null
      ? live.campaign_spent
      : ((cell.rollup || {}).total_cost_usd || 0);
    const ticket = live.ticket != null ? live.ticket : (cell.current_stage != null ? cell.current_stage : '?');
    const turn = live.turn != null ? live.turn : '?';
    const max = live.max_turns != null ? live.max_turns : '?';
    return {
      id: cell.id,
      text: `▶ Live · t${ticket} · ${turn}/${max} · $${Number(spent || 0).toFixed(4)}`,
      stalled: false,
    };
  }

  root.routeFor = routeFor;
  root.liveButton = liveButton;
})(typeof window !== 'undefined' ? window : globalThis);
