import { invoke } from '../lib/tauri'

export default function TitleBar({ title, onBack }) {
  return (
    <div className="titlebar" data-tauri-drag-region>
      <div className="titlebar-left">
        {onBack && (
          <button className="titlebar-back" onClick={onBack} title="Back">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
              <polyline points="15 18 9 12 15 6"/>
            </svg>
          </button>
        )}
        <span className="titlebar-title">{title}</span>
      </div>
      <div className="titlebar-controls">
        <button className="wc-btn wc-min" onClick={() => invoke('window_minimize')} title="Minimise">
          <svg viewBox="0 0 10 10"><line x1="1" y1="5" x2="9" y2="5" stroke="currentColor" strokeWidth="1.2"/></svg>
        </button>
        <button className="wc-btn wc-max" onClick={() => invoke('window_maximize')} title="Maximise">
          <svg viewBox="0 0 10 10"><rect x="1.5" y="1.5" width="7" height="7" stroke="currentColor" strokeWidth="1.2" fill="none"/></svg>
        </button>
        <button className="wc-btn wc-close" onClick={() => invoke('window_close')} title="Close">
          <svg viewBox="0 0 10 10">
            <line x1="1.5" y1="1.5" x2="8.5" y2="8.5" stroke="currentColor" strokeWidth="1.2"/>
            <line x1="8.5" y1="1.5" x2="1.5" y2="8.5" stroke="currentColor" strokeWidth="1.2"/>
          </svg>
        </button>
      </div>
    </div>
  )
}
