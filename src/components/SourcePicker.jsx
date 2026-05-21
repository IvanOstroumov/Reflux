// SourcePicker.jsx
// Modal for choosing which monitor/window to capture. Standalone component
// (previously this file circularly re-exported App, which would crash at
// import time). Receives the source list and select/cancel callbacks.

export default function SourcePicker({ sources, onSelect, onCancel }) {
  return (
    <div
      className="modal-backdrop"
      onClick={e => e.target === e.currentTarget && onCancel()}
    >
      <div className="modal">
        <div className="modal-title">Select Capture Source</div>
        <div className="modal-list">
          {sources.length === 0 && (
            <div className="modal-empty">
              No sources found — are you running outside Windows?
            </div>
          )}
          {sources.map(src => (
            <button
              key={src.id}
              className="source-item"
              onClick={() => onSelect(src)}
            >
              <span className="source-icon">
                {src.kind === 'monitor' ? '🖥' : '🎮'}
              </span>
              <div>
                <div className="source-name">{src.name}</div>
                <div className="source-meta">
                  {src.kind} · {src.width}×{src.height}
                </div>
              </div>
            </button>
          ))}
        </div>
        <button className="btn-ghost" onClick={onCancel}>Cancel</button>
      </div>
    </div>
  )
}
