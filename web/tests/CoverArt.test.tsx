import { fireEvent, render, screen, waitFor } from '@solidjs/testing-library'
import { createSignal } from 'solid-js'
import { describe, expect, it } from 'vitest'
import { CoverArt } from '../src/components/CoverArt'

function CoverHarness() {
  const [id, setId] = createSignal<string>()
  return (
    <>
      <CoverArt id={id()} alt="测试封面" kind="track" />
      <button onClick={() => setId('img-track-1')}>设置封面</button>
      <button onClick={() => setId('img-track-2')}>切换封面</button>
    </>
  )
}

describe('CoverArt', () => {
  it('loads a cover when an id becomes available', async () => {
    render(() => <CoverHarness />)
    expect(screen.queryByRole('img', { name: '测试封面' })).toBeNull()

    await fireEvent.click(screen.getByRole('button', { name: '设置封面' }))

    await waitFor(() => expect(screen.getByRole('img', { name: '测试封面' })).toHaveAttribute('src', '/rest/getCoverArt?v=1.16.1&c=mNest&id=img-track-1'))
  })

  it('retries after a failed cover when the track changes', async () => {
    render(() => <CoverHarness />)
    await fireEvent.click(screen.getByRole('button', { name: '设置封面' }))
    await fireEvent.error(screen.getByRole('img', { name: '测试封面' }))
    expect(screen.queryByRole('img', { name: '测试封面' })).toBeNull()

    await fireEvent.click(screen.getByRole('button', { name: '切换封面' }))

    await waitFor(() => expect(screen.getByRole('img', { name: '测试封面' })).toHaveAttribute('src', '/rest/getCoverArt?v=1.16.1&c=mNest&id=img-track-2'))
  })
})
