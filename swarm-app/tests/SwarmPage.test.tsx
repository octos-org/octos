/**
 * M7.6 acceptance tests for the contract-authoring + swarm dispatch
 * dashboard. Vitest + React Testing Library.
 *
 * Covers:
 *   1. 4-tab layout renders.
 *   2. Contract editor rejects malformed contracts.
 *   3. Dispatch form POSTs to `/api/swarm/dispatch`.
 *   4. Live view surfaces a swarm dispatch event over SSE.
 *   5. Review gate submits a decision and flips to the accepted state.
 */

import { describe, expect, it, beforeEach, afterEach, vi } from 'vitest'
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import SwarmPage from '../src/pages/SwarmPage'
import ContractEditor from '../src/components/swarm/ContractEditor'
import DispatchForm from '../src/components/swarm/DispatchForm'
import ReviewGate from '../src/components/swarm/ReviewGate'
import LiveView from '../src/components/swarm/LiveView'

type StubEventSource = EventSource & {
  onmessage: ((ev: MessageEvent) => void) | null
  onerror: ((ev: Event) => void) | null
  onopen: ((ev: Event) => void) | null
  dispatch: (payload: unknown) => void
}

// ── Fetch mocking helpers ──

function mockFetchSequence(
  responses: Array<{ url: string | RegExp; body: unknown; status?: number }>,
) {
  const calls: Array<{ url: string; init?: RequestInit }> = []
  const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
    calls.push({ url, init })
    // Prefer exact matches before prefix matches so e.g.
    // `/api/swarm/dispatches/abc` lands on its own mock rather than
    // the `/api/swarm/dispatches` list mock.
    const exact = responses.find(
      (r) => typeof r.url === 'string' && url === r.url,
    )
    const match =
      exact ||
      responses.find((r) =>
        typeof r.url === 'string'
          ? url.startsWith(r.url)
          : r.url.test(url),
      )
    if (!match) {
      return new Response(JSON.stringify({ error: `no mock for ${url}` }), {
        status: 404,
      })
    }
    return new Response(JSON.stringify(match.body), {
      status: match.status ?? 200,
      headers: { 'content-type': 'application/json' },
    })
  })
  // @ts-expect-error - overriding global fetch for the test
  global.fetch = fetchMock
  return { fetchMock, calls }
}

beforeEach(() => {
  localStorage.clear()
})

afterEach(() => {
  vi.restoreAllMocks()
})

describe('SwarmPage', () => {
  it('should render 4 tab panels', async () => {
    mockFetchSequence([
      { url: '/api/swarm/dispatches', body: { dispatches: [] } },
    ])
    render(<SwarmPage />)
    expect(screen.getByTestId('swarm-page')).toBeInTheDocument()
    expect(screen.getByTestId('swarm-tab-author')).toBeInTheDocument()
    expect(screen.getByTestId('swarm-tab-dispatch')).toBeInTheDocument()
    expect(screen.getByTestId('swarm-tab-live')).toBeInTheDocument()
    expect(screen.getByTestId('swarm-tab-review')).toBeInTheDocument()
    // Author panel is active by default.
    expect(screen.getByTestId('swarm-panel-author')).toBeInTheDocument()
  })

  it('should reject malformed contract in editor', async () => {
    // Drive a malformed body directly through the controlled `value`
    // prop — this is the shape SwarmPage uses and exercises the parse
    // + validator path end-to-end without leaning on userEvent's
    // keyboard parser (which treats `{` as a special key descriptor).
    const { rerender } = render(
      <ContractEditor value="" onChange={() => {}} />,
    )
    rerender(
      <ContractEditor
        value="{ this is not json "
        onChange={() => {}}
      />,
    )
    await waitFor(() => {
      expect(screen.getByTestId('swarm-editor-parse-error')).toBeInTheDocument()
    })
    // And a well-formed-but-incomplete contract still surfaces the
    // validation issues rail (no dispatch_id, empty contracts).
    rerender(
      <ContractEditor value="{}" onChange={() => {}} />,
    )
    await waitFor(() => {
      expect(screen.getByTestId('swarm-editor-issues')).toBeInTheDocument()
    })
  })

  it('should POST dispatch form on submit', async () => {
    const { fetchMock, calls } = mockFetchSequence([
      {
        url: '/api/swarm/dispatch',
        body: {
          dispatch_id: 'd-1',
          outcome: 'success',
          total_subtasks: 2,
          completed_subtasks: 2,
        },
      },
    ])
    const body = JSON.stringify({
      schema_version: 1,
      dispatch_id: 'd-1',
      contract_id: 'contract-a',
      contracts: [
        {
          contract_id: 'sub-1',
          tool_name: 'claude_code/run_task',
          task: { prompt: 'hi' },
        },
      ],
      topology: { kind: 'parallel', max_concurrency: 1 },
      budget: {},
    })
    const user = userEvent.setup()
    const onDispatched = vi.fn()
    render(<DispatchForm contractBody={body} onDispatched={onDispatched} />)
    await user.click(screen.getByTestId('swarm-dispatch-submit'))
    await waitFor(() => {
      expect(fetchMock).toHaveBeenCalled()
    })
    expect(calls[0].url).toBe('/api/swarm/dispatch')
    expect(onDispatched).toHaveBeenCalledWith(
      expect.objectContaining({ dispatch_id: 'd-1' }),
    )
    await waitFor(() => {
      expect(screen.getByTestId('swarm-dispatch-result')).toHaveTextContent('d-1')
    })
  })

  it('should show live progress for running swarm', async () => {
    mockFetchSequence([
      {
        url: '/api/swarm/dispatches',
        body: {
          dispatches: [
            {
              dispatch_id: 'live-1',
              contract_id: 'contract-a',
              topology: 'parallel',
              outcome: 'partial',
              total_subtasks: 3,
              completed_subtasks: 1,
              retry_rounds_used: 0,
              created_at: new Date().toISOString(),
            },
          ],
        },
      },
      {
        url: '/api/swarm/dispatches/live-1',
        body: {
          schema_version: 1,
          dispatch_id: 'live-1',
          contract_id: 'contract-a',
          topology: 'parallel',
          outcome: 'partial',
          total_subtasks: 3,
          completed_subtasks: 1,
          retry_rounds_used: 0,
          finalized: false,
          subtasks: [
            {
              contract_id: 'sub-1',
              status: 'completed',
              attempts: 1,
              last_dispatch_outcome: 'success',
              output: '',
            },
            {
              contract_id: 'sub-2',
              status: 'retryable_failed',
              attempts: 2,
              last_dispatch_outcome: 'timeout',
              output: '',
            },
            {
              contract_id: 'sub-3',
              status: 'retryable_failed',
              attempts: 2,
              last_dispatch_outcome: 'timeout',
              output: '',
            },
          ],
          validator_evidence: [],
          cost_attributions: [],
          total_cost_usd: 0,
        },
      },
    ])
    render(<LiveView />)
    // SSE-bound state flip — simulate a dispatch event through the stub.
    await waitFor(() => {
      // Each component render creates one EventSource; we keep the
      // most-recent one for simulation below.
      expect(
        (
          globalThis as unknown as {
            StubEventSource: { instances: StubEventSource[] }
          }
        ).StubEventSource.instances.length,
      ).toBeGreaterThan(0)
    })
    const stubs = (
      globalThis as unknown as {
        StubEventSource: { instances: StubEventSource[] }
      }
    ).StubEventSource.instances
    const es = stubs[stubs.length - 1]
    act(() => {
      es.dispatch({
        kind: 'swarm_dispatch',
        dispatch_id: 'live-1',
        contract_id: 'contract-a',
        topology: 'parallel',
        outcome: 'partial',
        total_subtasks: 3,
        completed_subtasks: 1,
        retry_round: 0,
      })
    })
    await waitFor(() => {
      expect(screen.getByTestId('swarm-live-row-live-1')).toBeInTheDocument()
    })
  })

  it('should submit review decision and show accepted state', async () => {
    const { fetchMock } = mockFetchSequence([
      {
        url: '/api/swarm/dispatches',
        body: {
          dispatches: [
            {
              dispatch_id: 'review-1',
              contract_id: 'contract-a',
              topology: 'parallel',
              outcome: 'success',
              total_subtasks: 1,
              completed_subtasks: 1,
              retry_rounds_used: 0,
              created_at: new Date().toISOString(),
            },
          ],
        },
      },
      {
        url: '/api/swarm/dispatches/review-1',
        body: {
          schema_version: 1,
          dispatch_id: 'review-1',
          contract_id: 'contract-a',
          topology: 'parallel',
          outcome: 'success',
          total_subtasks: 1,
          completed_subtasks: 1,
          retry_rounds_used: 0,
          finalized: true,
          subtasks: [],
          validator_evidence: [
            { name: 'harness_contract', passed: true, message: null },
          ],
          cost_attributions: [],
          total_cost_usd: 0,
        },
      },
      {
        url: '/api/cost/attributions/review-1',
        body: {
          dispatch_id: 'review-1',
          attributions: [],
          total_cost_usd: 0,
          total_tokens_in: 0,
          total_tokens_out: 0,
          count: 0,
        },
      },
      {
        url: '/api/swarm/dispatches/review-1/review',
        body: {
          dispatch_id: 'review-1',
          accepted: true,
          reviewer: 'ychen@futurewei.com',
          schema_version: 1,
        },
      },
    ])
    const user = userEvent.setup()
    render(<ReviewGate />)
    await waitFor(() =>
      expect(screen.getByTestId('swarm-review-dispatch-review-1')).toBeInTheDocument(),
    )
    await user.click(screen.getByTestId('swarm-review-dispatch-review-1'))
    await waitFor(() =>
      expect(
        screen.getByTestId('swarm-review-validator-harness_contract'),
      ).toBeInTheDocument(),
    )
    await user.type(
      screen.getByTestId('swarm-review-reviewer'),
      'ychen@futurewei.com',
    )
    await user.click(screen.getByTestId('swarm-review-accept'))
    await waitFor(() => {
      expect(screen.getByTestId('swarm-review-accepted-state')).toBeInTheDocument()
    })
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/swarm/dispatches/review-1/review',
      expect.objectContaining({ method: 'POST' }),
    )
  })
})
