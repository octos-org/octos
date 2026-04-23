import '@testing-library/jest-dom/vitest'
import { afterEach } from 'vitest'
import { cleanup } from '@testing-library/react'

// Keep the DOM pristine between tests.
afterEach(() => {
  cleanup()
})

// jsdom does not implement EventSource — stub the minimum surface the
// LiveView component consumes so tests don't crash.
class StubEventSource {
  static instances: StubEventSource[] = []
  url: string
  onmessage: ((ev: MessageEvent) => void) | null = null
  onerror: ((ev: Event) => void) | null = null
  onopen: ((ev: Event) => void) | null = null
  readyState = 1
  constructor(url: string) {
    this.url = url
    StubEventSource.instances.push(this)
  }
  close() {
    this.readyState = 2
  }
  dispatch(payload: unknown) {
    if (this.onmessage) {
      this.onmessage(
        new MessageEvent('message', { data: JSON.stringify(payload) }),
      )
    }
  }
}
;(globalThis as unknown as { EventSource: typeof StubEventSource }).EventSource =
  StubEventSource
;(globalThis as unknown as { StubEventSource?: typeof StubEventSource }).StubEventSource =
  StubEventSource

// jsdom lacks scrollIntoView for some versions of RTL queries.
if (!window.HTMLElement.prototype.scrollIntoView) {
  window.HTMLElement.prototype.scrollIntoView = () => {}
}
