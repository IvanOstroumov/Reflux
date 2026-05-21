export default function StatsOverlay({ stats, role, onClose }) {
  const rows = [
    ['STREAM FPS',  `${stats.fps} fps`],
    ['BITRATE',     `${stats.bitrate} Mbps`],
    ['RTT',         stats.rtt == null ? '—' : `${stats.rtt} ms`],
    ['PACKET LOSS', stats.loss == null
      ? '—'
      : stats.loss > 0
        ? <span style={{ color: 'var(--red)' }}>{stats.loss}%</span>
        : '0%'
    ],
    ['RESOLUTION',  stats.resolution || '—'],
    ['ENCODER',     stats.encoder || '—'],
    ['CAPTURE',     stats.captureMethod || '—'],
    ['TRANSPORT',   stats.connType || '—'],
    ...(role === 'viewer' ? [
      ['FRAMES DECODED', stats.framesDecoded || 0],
      ['DATA RECEIVED',  `${stats.mb || 0} MB`],
    ] : []),
  ]

  return (
    <div className="stats-overlay">
      <div className="stats-header">
        <span className="stats-title">⬛ STREAM STATS</span>
        <button className="stats-close" onClick={onClose}>✕</button>
      </div>
      {rows.map(([label, value]) => (
        <div key={label} className="stats-row">
          <span className="stats-label">{label}</span>
          <span className="stats-value">{value}</span>
        </div>
      ))}
    </div>
  )
}
