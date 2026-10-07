// Shared leaf helpers for the GUI (index.html) and the static portal.
// Single source: index.html loads this file; portal.rs inlines it at export.
// Keep dependency-free and side-effect-free.
function escapeHtml(s) {
  if (s === null || s === undefined) return '';
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#039;');
}

function shortModelName(modelName) {
  if (!modelName) return '';
  return modelName.split('/').pop() || modelName;
}

function getModelMeta(modelName) {
  const m = (modelName || '').toLowerCase();
  if (m.includes('claude') || m.includes('anthropic')) {
    return { family: 'Anthropic', icon: '✦', color: '#f27244', bg: 'rgba(242, 114, 68, 0.14)', border: 'rgba(242, 114, 68, 0.35)' };
  }
  if (m.includes('gpt') || m.includes('openai') || m.includes('o1') || m.includes('o3') || m.includes('o4')) {
    return { family: 'OpenAI', icon: '✳️', color: '#10a37f', bg: 'rgba(16, 163, 127, 0.14)', border: 'rgba(16, 163, 127, 0.35)' };
  }
  if (m.includes('gemini') || m.includes('google')) {
    return { family: 'Google', icon: '✨', color: '#4285f4', bg: 'rgba(66, 133, 244, 0.14)', border: 'rgba(66, 133, 244, 0.35)' };
  }
  if (m.includes('deepseek')) {
    return { family: 'DeepSeek', icon: '🐋', color: '#0084ff', bg: 'rgba(0, 132, 255, 0.14)', border: 'rgba(0, 132, 255, 0.35)' };
  }
  if (m.includes('llama') || m.includes('meta')) {
    return { family: 'Meta', icon: '🦙', color: '#8b5cf6', bg: 'rgba(139, 92, 246, 0.14)', border: 'rgba(139, 92, 246, 0.35)' };
  }
  if (m.includes('qwen')) {
    return { family: 'Qwen', icon: '🔮', color: '#a855f7', bg: 'rgba(168, 85, 247, 0.14)', border: 'rgba(168, 85, 247, 0.35)' };
  }
  if (m.includes('mistral') || m.includes('codestral') || m.includes('pixtral')) {
    return { family: 'Mistral', icon: '🌪️', color: '#ff7000', bg: 'rgba(255, 112, 0, 0.14)', border: 'rgba(255, 112, 0, 0.35)' };
  }
  if (m.includes('minimax')) {
    return { family: 'MiniMax', icon: '⚡', color: '#e11d48', bg: 'rgba(225, 29, 72, 0.14)', border: 'rgba(225, 29, 72, 0.35)' };
  }
  if (m.includes('nova') || m.includes('amazon')) {
    return { family: 'Amazon', icon: '☀️', color: '#f59e0b', bg: 'rgba(245, 158, 11, 0.14)', border: 'rgba(245, 158, 11, 0.35)' };
  }
  if (m.includes('cohere') || m.includes('command')) {
    return { family: 'Cohere', icon: '🌿', color: '#14b8a6', bg: 'rgba(20, 184, 166, 0.14)', border: 'rgba(20, 184, 166, 0.35)' };
  }
  let hash = 0;
  for (let i = 0; i < m.length; i++) hash = (hash * 31 + m.charCodeAt(i)) & 0xffffffff;
  const PALETTE = [
    { color: '#06b6d4', bg: 'rgba(6, 182, 212, 0.14)', border: 'rgba(6, 182, 212, 0.35)' },
    { color: '#ec4899', bg: 'rgba(236, 72, 153, 0.14)', border: 'rgba(236, 72, 153, 0.35)' },
    { color: '#84cc16', bg: 'rgba(132, 204, 22, 0.14)', border: 'rgba(132, 204, 22, 0.35)' },
    { color: '#3b82f6', bg: 'rgba(59, 130, 246, 0.14)', border: 'rgba(59, 130, 246, 0.35)' },
    { color: '#f43f5e', bg: 'rgba(244, 63, 94, 0.14)', border: 'rgba(244, 63, 94, 0.35)' },
    { color: '#6366f1', bg: 'rgba(99, 102, 241, 0.14)', border: 'rgba(99, 102, 241, 0.35)' },
    { color: '#10b981', bg: 'rgba(16, 185, 129, 0.14)', border: 'rgba(16, 185, 129, 0.35)' },
    { color: '#f59e0b', bg: 'rgba(245, 158, 11, 0.14)', border: 'rgba(245, 158, 11, 0.35)' }
  ];
  const choice = PALETTE[Math.abs(hash) % PALETTE.length];
  return { family: 'Other', icon: '🤖', color: choice.color, bg: choice.bg, border: choice.border };
}

// Units are not interchangeable: pubsub reports fan-out *deliveries*
// (200 subs x 20 msgs = 4000 copies from 20 requests), wal reports
// fsync-bound messages, binproto reports binary ops. Any of those labelled
// "req/s" invites comparing them against a real request rate, which the
// numbers do not support. Mirrors throughput_unit() in leaderboard.rs.
function formatThroughput(tp, task) {
  if (!tp) return 'N/A';
  const t = (task || '').toLowerCase();
  const unit = t === 'pdf' ? 'pages/s'
    : t === 'wal' ? 'msgs/s'
    : t === 'pubsub' ? 'deliveries/s'
    : t === 'binproto' ? 'ops/s'
    : 'req/s';
  return `${Math.round(tp).toLocaleString()} ${unit}`;
}
