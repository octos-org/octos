import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function PptConfigTab({ config, onChange }: Props) {
  const getSlidesValue = (key: 'template_dir' | 'default_theme', legacyKey: string) =>
    config.apps?.slides?.[key] ?? config.env_vars?.[legacyKey] ?? ''

  const updateSlides = (key: 'template_dir' | 'default_theme', value: string) => {
    const normalized = value.trim()
    const nextSlides = {
      ...(config.apps?.slides ?? {}),
      [key]: normalized || undefined,
    }

    const hasSlidesValues = Object.values(nextSlides).some((entry) => entry != null && entry !== '')

    onChange({
      ...config,
      apps: hasSlidesValues
        ? { ...(config.apps ?? {}), slides: nextSlides }
        : config.apps
          ? { ...config.apps, slides: undefined }
          : undefined,
    })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">PPT Generation</p>
        <p>
          Configure first-party slides settings in{' '}
          <code className="bg-gray-800 px-1 rounded">config.apps.slides</code>. This keeps slide
          behavior typed and separate from generic env var editing.
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">
          PPT Template Directory
        </label>
        <input
          value={getSlidesValue('template_dir', 'PPT_TEMPLATE_DIR')}
          onChange={(e) => updateSlides('template_dir', e.target.value)}
          placeholder="/path/to/ppt/templates"
          className="input text-xs"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Legacy env fallback: <code className="bg-gray-800 px-1 rounded">PPT_TEMPLATE_DIR</code>
        </p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">
          Default Theme
        </label>
        <input
          value={getSlidesValue('default_theme', 'PPT_DEFAULT_THEME')}
          onChange={(e) => updateSlides('default_theme', e.target.value)}
          placeholder="default"
          className="input text-xs"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          Legacy env fallback: <code className="bg-gray-800 px-1 rounded">PPT_DEFAULT_THEME</code>
        </p>
      </div>
    </div>
  )
}
