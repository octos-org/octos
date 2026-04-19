import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function DeepCrawlTab({ config, onChange }: Props) {
  const updateDeepCrawl = (
    key: 'page_settle_ms' | 'max_output_chars',
    value: string,
  ) => {
    const normalized = value.trim()
    const parsed = normalized === '' ? undefined : Number.parseInt(normalized, 10)
    const nextDeepCrawl = {
      ...(config.deep_crawl ?? {}),
      [key]: Number.isFinite(parsed) ? parsed : undefined,
    }

    onChange({
      ...config,
      deep_crawl: Object.values(nextDeepCrawl).some((entry) => entry != null)
        ? nextDeepCrawl
        : undefined,
    })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Deep Crawl</p>
        <p>
          Configure the structured <code className="bg-gray-800 px-1 rounded">config.deep_crawl</code>{' '}
          section used by the <code className="bg-gray-800 px-1 rounded">deep_crawl</code> tool.
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Page Settle Time (ms)</label>
        <input
          type="number"
          min={500}
          max={10000}
          step={100}
          value={config.deep_crawl?.page_settle_ms ?? ''}
          onChange={(e) => updateDeepCrawl('page_settle_ms', e.target.value)}
          placeholder="3000"
          className="input max-w-[160px]"
        />
        <p className="text-xs text-gray-600 mt-1">
          How long the crawler waits for client-side rendering before extracting content. Default: 3000.
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max Output Characters</label>
        <input
          type="number"
          min={10000}
          max={200000}
          step={1000}
          value={config.deep_crawl?.max_output_chars ?? ''}
          onChange={(e) => updateDeepCrawl('max_output_chars', e.target.value)}
          placeholder="50000"
          className="input max-w-[160px]"
        />
        <p className="text-xs text-gray-600 mt-1">
          Truncation limit for extracted crawl output. Default: 50000.
        </p>
      </div>
    </div>
  )
}
