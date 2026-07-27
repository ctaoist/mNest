import { ApiError, post, refreshSession } from './api'

export interface NeteaseAudioMatchResult {
  id: string
  title: string
  artists: string[]
  album: string
  start_time_ms: number
}

type FingerprintRuntime = typeof globalThis & {
  GenerateFP?: (samples: Float32Array) => Promise<string>
}

const RUNTIME_URL = '/api/remote_download/netease/audio-match/runtime.js'
let runtimePromise: Promise<(samples: Float32Array) => Promise<string>> | undefined

export async function identifyNeteaseAudio(samples: Float32Array): Promise<NeteaseAudioMatchResult[]> {
  if (samples.length !== 24_000) throw new Error('听歌识曲需要3秒、8kHz的音频样本')
  const generateFingerprint = await loadFingerprintRuntime()
  const audioFP = await generateFingerprint(samples)
  if (!audioFP) throw new Error('网易云音频指纹生成失败')
  return post<NeteaseAudioMatchResult[]>('/api/remote_download/netease/audio-match/', {
    duration: 3,
    audio_fp: audioFP,
  })
}

async function loadFingerprintRuntime() {
  runtimePromise ||= (async () => {
    const source = await fetchRuntimeSource()
    const scriptUrl = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }))
    try {
      await new Promise<void>((resolve, reject) => {
        const script = document.createElement('script')
        script.src = scriptUrl
        script.onload = () => {
          script.remove()
          resolve()
        }
        script.onerror = () => {
          script.remove()
          reject(new Error('网易云听歌识曲运行时加载失败'))
        }
        document.head.append(script)
      })
    } finally {
      URL.revokeObjectURL(scriptUrl)
    }
    const generateFingerprint = (globalThis as FingerprintRuntime).GenerateFP
    if (typeof generateFingerprint !== 'function') throw new Error('网易云听歌识曲运行时无效')
    return generateFingerprint
  })().catch((error) => {
    runtimePromise = undefined
    throw error
  })
  return runtimePromise
}

async function fetchRuntimeSource(retry = true): Promise<string> {
  const response = await fetch(RUNTIME_URL, { credentials: 'same-origin' })
  if (response.status === 401 && retry && await refreshSession()) return fetchRuntimeSource(false)
  if (!response.ok) {
    const body = await response.json().catch(() => ({})) as { message?: string; error?: string }
    throw new ApiError(body.message || body.error || `听歌识曲运行时加载失败 (${response.status})`, response.status)
  }
  return response.text()
}
