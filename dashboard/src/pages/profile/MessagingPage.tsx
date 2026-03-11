import { useState } from 'react'
import { useProfile } from '../../contexts/ProfileContext'
import CategoryTabs from '../../components/CategoryTabs'
import SaveFooter from '../../components/SaveFooter'
import TelegramTab from '../../components/tabs/TelegramTab'
import DiscordTab from '../../components/tabs/DiscordTab'
import WhatsAppTab from '../../components/tabs/WhatsAppTab'
import FeishuTab from '../../components/tabs/FeishuTab'
import WeComBotTab from '../../components/tabs/WeComBotTab'

const TABS = [
  { key: 'telegram', label: 'Telegram' },
  { key: 'discord', label: 'Discord' },
  { key: 'whatsapp', label: 'WhatsApp' },
  { key: 'feishu', label: 'Feishu' },
  { key: 'wecom-bot', label: 'WeCom' },
]

export default function MessagingPage() {
  const { config, setConfig, save, saving, loading, status, profileId } = useProfile()
  const [activeTab, setActiveTab] = useState('telegram')

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin w-6 h-6 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  const isRunning = status?.running === true

  return (
    <div>
      <h1 className="text-2xl font-bold text-white mb-6">Messaging</h1>
      <div className="bg-surface rounded-xl border border-gray-700/50 overflow-hidden">
        <div className="px-5 pt-4">
          <CategoryTabs tabs={TABS} activeTab={activeTab} onTabChange={setActiveTab} />
        </div>
        <div className="p-5">
          {activeTab === 'telegram' && <TelegramTab config={config} onChange={setConfig} />}
          {activeTab === 'discord' && <DiscordTab config={config} onChange={setConfig} />}
          {activeTab === 'whatsapp' && <WhatsAppTab config={config} onChange={setConfig} isRunning={isRunning} />}
          {activeTab === 'feishu' && <FeishuTab config={config} onChange={setConfig} profileId={profileId} />}
          {activeTab === 'wecom-bot' && <WeComBotTab config={config} onChange={setConfig} />}
        </div>
        <SaveFooter onSave={save} saving={saving} />
      </div>
    </div>
  )
}
