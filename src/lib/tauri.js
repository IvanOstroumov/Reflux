// tauri.js
// Wraps @tauri-apps/api so the UI can be developed in a regular browser
// (with stubs) and runs for real inside the Tauri WebView.

const IS_TAURI = typeof window !== 'undefined' && window.__TAURI_INTERNALS__ !== undefined

let _invoke = null
let _listen = null
let _event = null

async function loadTauri() {
  if (!IS_TAURI) return
  if (_invoke) return
  const { invoke } = await import('@tauri-apps/api/core')
  const { listen, emit } = await import('@tauri-apps/api/event')
  _invoke = invoke
  _listen = listen
  _event = emit
}

export async function invoke(cmd, args = {}) {
  await loadTauri()
  if (!_invoke) {
    console.warn(`[stub] invoke: ${cmd}`, args)
    return stubInvoke(cmd, args)
  }
  return _invoke(cmd, args)
}

export async function listen(event, callback) {
  await loadTauri()
  if (!_listen) {
    console.warn(`[stub] listen: ${event}`)
    return () => {} // no-op unlisten
  }
  return _listen(event, callback)
}

export async function emit(event, payload) {
  await loadTauri()
  if (!_event) {
    console.warn(`[stub] emit: ${event}`, payload)
    return
  }
  return _event(event, payload)
}

// ─── Browser stubs for development ──────────────────────────────────────────

const STUB_SOURCES = [
  { id: 'monitor:0', name: 'Display 1 (1920×1080)', kind: 'monitor', width: 1920, height: 1080 },
  { id: 'monitor:1', name: 'Display 2 (2560×1440)', kind: 'monitor', width: 2560, height: 1440 },
  { id: 'window:1001', name: 'Escape from Tarkov', kind: 'window', width: 1920, height: 1080 },
  { id: 'window:1002', name: 'Counter-Strike 2', kind: 'window', width: 1920, height: 1080 },
  { id: 'window:1003', name: 'VALORANT', kind: 'window', width: 1920, height: 1080 },
]

async function stubInvoke(cmd, args) {
  await delay(120)
  switch (cmd) {
    case 'list_capture_sources':
      return STUB_SOURCES

    case 'start_host_session': {
      // Simulate async pipeline startup then auto-fire events
      setTimeout(() => dispatchStubEvent('session-status', { status: 'waiting_for_viewer', invite_code: 'R9KM-TZQ4' }), 400)
      setTimeout(() => dispatchStubEvent('session-status', { status: 'connecting' }), 3200)
      setTimeout(() => dispatchStubEvent('session-status', { status: 'streaming' }), 5000)
      return 'R9KM-TZQ4'
    }

    case 'join_viewer_session': {
      setTimeout(() => dispatchStubEvent('session-status', { status: 'connecting' }), 300)
      setTimeout(() => dispatchStubEvent('session-status', { status: 'streaming' }), 2500)
      return null
    }

    case 'stop_session':
      setTimeout(() => dispatchStubEvent('session-status', { status: 'idle' }), 100)
      return null

    case 'get_session_state':
      return null

    case 'window_minimize':
    case 'window_maximize':
    case 'window_close':
      return null

    default:
      console.warn(`[stub] unknown command: ${cmd}`)
      return null
  }
}

const _stubListeners = {}

function dispatchStubEvent(event, payload) {
  const handlers = _stubListeners[event] || []
  handlers.forEach(fn => fn({ payload }))
}

// Override listen for stubs
const _origListen = listen
export function stubListen(event, callback) {
  if (!_stubListeners[event]) _stubListeners[event] = []
  _stubListeners[event].push(callback)
  return () => {
    _stubListeners[event] = _stubListeners[event].filter(fn => fn !== callback)
  }
}

const delay = ms => new Promise(r => setTimeout(r, ms))
