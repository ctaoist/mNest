import { createEffect, createSignal, Show } from 'solid-js'
import { Disc3, Music2, RadioTower } from 'lucide-solid'
import { mediaUrl } from '../lib/api'

interface CoverArtProps {
  id?: string
  alt: string
  class?: string
  kind?: 'album' | 'track' | 'radio'
}

export function CoverArt(props: CoverArtProps) {
  const [failed, setFailed] = createSignal(!props.id)

  createEffect(() => {
    setFailed(!props.id)
  })

  return (
    <div class={`cover-art ${props.class || ''}`}>
      <Show when={!failed()} fallback={<div class="cover-fallback">{props.kind === 'radio' ? <RadioTower /> : props.kind === 'track' ? <Music2 /> : <Disc3 />}</div>}>
        <img src={mediaUrl('getCoverArt', { id: props.id })} alt={props.alt} loading={props.kind === 'track' ? 'eager' : 'lazy'} decoding="async" onError={() => setFailed(true)} />
      </Show>
    </div>
  )
}
