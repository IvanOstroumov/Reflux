const STATUS_COLORS = {
  idle: 'var(--text-faint)',
  waiting_for_viewer: 'var(--orange)',
  connecting: 'var(--orange)',
  streaming: 'var(--green)',
  reconnecting: 'var(--orange)',
  disconnected: 'var(--red)',
}

const STATUS_LABELS = {
  idle: 'Ready',
  waiting_for_viewer: 'Waiting for viewer',
  connecting: 'Connecting…',
  streaming: 'Live',
  reconnecting: 'Reconnecting…',
  disconnected: 'Disconnected',
}

export default function StatusBar({ status, stats, mode }) {
  const color = STATUS_COLORS[status] || 'var(--text-faint)'
  const label = STATUS_LABELS[status] || status

  return (
    <div className="statusbar">
      <span className="status-dot" style={{ background: color }} />
      <span className="status-label" style={{ color }}>{label}</span>

      {status === 'streaming' && (
        <>
          <div className="status-divider" />
          <span className="status-stat">{stats.fps} fps</span>
          <span className="status-stat">{stats.bitrate} Mbps</span>
          {stats.rtt != null && <span className="status-stat">{stats.rtt} ms</span>}
          {stats.loss != null && stats.loss > 0 && (
            <span className="status-stat" style={{ color: 'var(--red)' }}>
              {stats.loss}% loss
            </span>
          )}
        </>
      )}

      <div style={{ flex: 1 }} />
      <span className="status-mode">{mode ? mode.toUpperCase() : ''}</span>
    </div>
  )
}
